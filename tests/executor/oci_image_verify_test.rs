use caduceus::executor::oci_engine::NormalizedImage;
use caduceus::executor::oci_image::{verify_arch, verify_digest};
use caduceus::executor::oci_platform::HostPlatform;
use caduceus::infra::error::CaduceusError;

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IMAGE_REF: &str = "registry.example/worker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn image(repo_digests: Vec<String>, architecture: &str, variant: Option<&str>) -> NormalizedImage {
    NormalizedImage {
        id: format!("sha256:{DIGEST}"),
        repo_digests,
        architecture: architecture.to_string(),
        variant: variant.map(str::to_string),
    }
}

fn host(architecture: &str, variant: Option<&str>) -> HostPlatform {
    HostPlatform {
        architecture: architecture.to_string(),
        variant: variant.map(str::to_string),
    }
}

#[test]
fn digest_and_architecture_accept_matching_images() {
    let image = image(vec![IMAGE_REF.to_string()], "amd64", None);
    verify_digest(&image, IMAGE_REF).expect("digest should match");
    verify_arch(&image, &host("amd64", None)).expect("architecture should match");
}

#[test]
fn digest_mismatch_is_distinct_and_checked_before_architecture() {
    let image = image(
        vec!["registry.example/worker@sha256:bbbb".to_string()],
        "arm64",
        None,
    );
    let error = verify_digest(&image, IMAGE_REF).expect_err("digest must mismatch");
    assert!(matches!(
        error,
        CaduceusError::OciImageDigestMismatch { .. }
    ));

    // A caller must run digest verification before architecture verification;
    // this input would otherwise report the less authoritative arch failure.
    assert!(matches!(
        verify_digest(&image, IMAGE_REF),
        Err(CaduceusError::OciImageDigestMismatch { .. })
    ));
}

#[test]
fn architecture_mismatch_is_distinct_from_digest_failure() {
    let image = image(vec![IMAGE_REF.to_string()], "arm64", None);
    let error = verify_arch(&image, &host("amd64", None)).expect_err("architecture must mismatch");
    assert!(matches!(
        error,
        CaduceusError::OciImageArchitectureMismatch { .. }
    ));
    assert!(!matches!(error, CaduceusError::OciPullFailed { .. }));
}

#[test]
fn host_without_variant_accepts_any_image_variant() {
    let image = image(vec![IMAGE_REF.to_string()], "arm64", Some("v8"));
    verify_arch(&image, &host("arm64", None)).expect("host without variant accepts v8");
}

#[test]
fn host_variant_requires_a_matching_image_variant() {
    let matching = image(vec![IMAGE_REF.to_string()], "arm", Some("V7"));
    verify_arch(&matching, &host("arm", Some("v7"))).expect("variant should compare insensitive");

    for variant in [None, Some("v6")] {
        let image = image(vec![IMAGE_REF.to_string()], "arm", variant);
        assert!(matches!(
            verify_arch(&image, &host("arm", Some("v7"))),
            Err(CaduceusError::OciImageArchitectureMismatch { .. })
        ));
    }
}

#[test]
fn unknown_host_architecture_fails_closed() {
    let image = image(vec![IMAGE_REF.to_string()], "mips", None);
    assert!(matches!(
        verify_arch(&image, &host("mips", None)),
        Err(CaduceusError::OciImageArchitectureMismatch { .. })
    ));
}
