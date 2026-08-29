use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use caduceus::executor::oci_engine::OciImageAdapter;
use caduceus::executor::oci_image::ensure_image_with_adapter;
use caduceus::executor::oci_platform::HostPlatform;
use caduceus::executor::SandboxEngine;
use caduceus::infra::config::OciPullPolicy;

const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const IMAGE_REF: &str = "registry.example/worker@sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn fake_engine(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let binary = root.join("fake-oci");
    let calls = root.join("calls.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = \"pull\" ]; then\n  printf 'pull must not be called on a warm cache\\n' >&2\n  exit 91\nfi\nif [ \"$1\" = \"image\" ] && [ \"$2\" = \"inspect\" ] && [ \"$3\" = \"--format\" ]; then\n  printf '%s' '{{\"Id\":\"sha256:{DIGEST}\",\"RepoDigests\":[\"{IMAGE_REF}\"],\"Architecture\":\"amd64\"}}'\nfi\nexit 0\n",
        calls.display()
    );
    std::fs::write(&binary, script).expect("write fake OCI executable");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
        .expect("make fake OCI executable");
    (binary, calls)
}

#[tokio::test]
async fn if_missing_warm_cache_never_invokes_pull() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (binary, calls) = fake_engine(temp.path());
    let run_dir = temp.path().join("run");
    let image = ensure_image_with_adapter(
        OciImageAdapter::with_binary(SandboxEngine::Docker, binary),
        SandboxEngine::Docker,
        IMAGE_REF,
        OciPullPolicy::IfMissing,
        &HostPlatform {
            architecture: "amd64".to_string(),
            variant: None,
        },
        &run_dir,
        "warm-cache",
    )
    .await
    .expect("warm cache should verify");

    assert_eq!(image.id, format!("sha256:{DIGEST}"));
    let calls = std::fs::read_to_string(calls).expect("fake engine call log");
    assert!(calls.contains("image inspect"));
    assert!(!calls.lines().any(|line| line == "pull"), "calls: {calls}");
}
