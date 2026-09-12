#[path = "../fixtures/mod.rs"]
mod fixtures;

use std::path::{Path, PathBuf};

use caduceus::executor::oci_engine::OciImageAdapter;
use caduceus::executor::oci_image::ensure_image_with_adapter;
use caduceus::executor::oci_platform::HostPlatform;
use caduceus::executor::SandboxEngine;
use caduceus::infra::config::OciPullPolicy;
use caduceus::infra::error::CaduceusError;
use fixtures::write_executable_script;

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IMAGE_REF: &str = "registry.example/worker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn fake_engine(root: &Path, inspect_json: &str) -> (PathBuf, PathBuf) {
    let binary = root.join("fake-oci");
    let calls = root.join("calls.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = \"image\" ] && [ \"$2\" = \"inspect\" ] && [ \"$3\" != \"--format\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"image\" ] && [ \"$2\" = \"inspect\" ] && [ \"$3\" = \"--format\" ]; then\n  printf '%s' '{}'\n  exit 0\nfi\nif [ \"$1\" = \"pull\" ]; then\n  exit 0\nfi\nexit 97\n",
        calls.display(),
        inspect_json
    );
    write_executable_script(root, "fake-oci", &script);
    (binary, calls)
}

fn host() -> HostPlatform {
    HostPlatform {
        architecture: "amd64".to_string(),
        variant: None,
    }
}

#[tokio::test]
async fn digest_mismatch_is_returned_before_any_create_call() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inspect = format!(
        r#"{{"Id":"sha256:{DIGEST}","RepoDigests":["registry.example/worker@sha256:{wrong}"],"Architecture":"amd64"}}"#,
        wrong = "b".repeat(64)
    );
    let (binary, calls) = fake_engine(temp.path(), &inspect);
    let run_dir = temp.path().join("run");
    let error = ensure_image_with_adapter(
        OciImageAdapter::with_binary(SandboxEngine::Docker, binary),
        SandboxEngine::Docker,
        IMAGE_REF,
        OciPullPolicy::IfMissing,
        &host(),
        &run_dir,
        "digest-mismatch",
    )
    .await
    .expect_err("mismatched RepoDigests must abort");

    assert!(matches!(
        error,
        CaduceusError::OciImageDigestMismatch { .. }
    ));
    let calls = std::fs::read_to_string(calls).expect("fake engine call log");
    assert!(!calls.lines().any(|line| line.starts_with("create")));
    assert!(!calls.lines().any(|line| line.starts_with("pull")));
}
