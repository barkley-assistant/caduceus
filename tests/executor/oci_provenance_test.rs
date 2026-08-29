use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use caduceus::executor::oci_engine::OciImageAdapter;
use caduceus::executor::oci_image::ensure_image_with_adapter;
use caduceus::executor::oci_platform::HostPlatform;
use caduceus::executor::SandboxEngine;
use caduceus::infra::config::OciPullPolicy;
use caduceus::infra::error::CaduceusError;
use serde_json::Value;

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IMAGE_REF: &str = "registry.example/worker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn host() -> HostPlatform {
    HostPlatform {
        architecture: "amd64".to_string(),
        variant: None,
    }
}

fn fake_engine(root: &Path, local_present: bool, inspect_json: &str, pull_fails: bool) -> PathBuf {
    let binary = root.join("fake-oci");
    let present_exit = if local_present { "0" } else { "1" };
    let pull_body = if pull_fails {
        "printf 'registry offline\\n' >&2\n  exit 42"
    } else {
        "exit 0"
    };
    let inspect_json = inspect_json.replace('\\', "\\\\").replace('\'', "'\\''");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"image\" ] && [ \"$2\" = \"inspect\" ] && [ \"$3\" != \"--format\" ]; then\n  if [ \"{present_exit}\" = \"1\" ]; then\n    printf 'No such image\\n' >&2\n  fi\n  exit {present_exit}\nfi\nif [ \"$1\" = \"pull\" ]; then\n  {pull_body}\nfi\nif [ \"$1\" = \"image\" ] && [ \"$2\" = \"inspect\" ] && [ \"$3\" = \"--format\" ]; then\n  printf '%s' '{inspect_json}'\n  exit 0\nfi\nexit 97\n",
        present_exit = present_exit,
        pull_body = pull_body,
        inspect_json = inspect_json,
    );
    std::fs::write(&binary, script).expect("write fake OCI executable");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
        .expect("make fake OCI executable");
    binary
}

fn read_record(run_dir: &Path) -> Value {
    let contents = std::fs::read_to_string(run_dir.join("provenance.log"))
        .expect("provenance log should exist");
    let mut lines = contents.lines();
    let record: Value = serde_json::from_str(lines.next().expect("one provenance line"))
        .expect("provenance line must be JSON");
    assert!(lines.next().is_none(), "ensure_image must emit one record");
    record
}

fn assert_common_fields(record: &Value) {
    for field in [
        "reference",
        "engine",
        "policy",
        "mode",
        "stage",
        "outcome",
        "timings",
    ] {
        assert!(!record[field].is_null(), "field {field} must be present");
    }
    assert_eq!(record["reference"], IMAGE_REF);
    assert_eq!(record["engine"], "docker");
    assert!(record["timings"].is_object());
}

#[tokio::test]
async fn successful_acquisition_is_audited_with_resolved_fields() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inspect = format!(
        r#"{{"Id":"sha256:{DIGEST}","RepoDigests":["{IMAGE_REF}"],"Architecture":"amd64"}}"#
    );
    let binary = fake_engine(temp.path(), true, &inspect, false);
    let run_dir = temp.path().join("success");
    ensure_image_with_adapter(
        OciImageAdapter::with_binary(SandboxEngine::Docker, binary),
        SandboxEngine::Docker,
        IMAGE_REF,
        OciPullPolicy::IfMissing,
        &host(),
        &run_dir,
        "success-run",
    )
    .await
    .expect("matching image should verify");

    let record = read_record(&run_dir);
    assert_common_fields(&record);
    assert_eq!(record["run_id"], "success-run");
    assert_eq!(record["resolved_id"], format!("sha256:{DIGEST}"));
    assert_eq!(record["repo_digests"].as_array().map(Vec::len), Some(1));
    assert_eq!(record["architecture"], "amd64");
    assert_eq!(record["policy"], "if_missing");
    assert_eq!(record["mode"], "local");
    assert_eq!(record["cache_hit"], true);
    assert_eq!(record["pull_attempted"], false);
    assert_eq!(record["stage"], "acquired");
    assert_eq!(record["outcome"], "ok");
    assert_eq!(record["verification"]["status"], "passed");
}

#[tokio::test]
async fn verification_abort_is_audited_with_null_resolved_fields() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inspect = format!(
        r#"{{"Id":"sha256:{DIGEST}","RepoDigests":["registry.example/worker@sha256:{wrong}"],"Architecture":"amd64"}}"#,
        wrong = "b".repeat(64)
    );
    let binary = fake_engine(temp.path(), true, &inspect, false);
    let run_dir = temp.path().join("digest-abort");
    let error = ensure_image_with_adapter(
        OciImageAdapter::with_binary(SandboxEngine::Docker, binary),
        SandboxEngine::Docker,
        IMAGE_REF,
        OciPullPolicy::IfMissing,
        &host(),
        &run_dir,
        "digest-abort-run",
    )
    .await
    .expect_err("digest mismatch should abort");
    assert!(matches!(
        error,
        CaduceusError::OciImageDigestMismatch { .. }
    ));

    let record = read_record(&run_dir);
    assert_common_fields(&record);
    assert_eq!(record["resolved_id"], Value::Null);
    assert_eq!(record["repo_digests"], Value::Null);
    assert_eq!(record["architecture"], Value::Null);
    assert_eq!(record["variant"], Value::Null);
    assert_eq!(record["stage"], "digest_verify");
    assert_eq!(record["outcome"], "aborted");
    assert_eq!(record["error"]["variant"], "OciImageDigestMismatch");
    assert_eq!(record["verification"]["status"], "failed");
}

#[tokio::test]
async fn architecture_abort_is_audited_at_the_architecture_stage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inspect = format!(
        r#"{{"Id":"sha256:{DIGEST}","RepoDigests":["{IMAGE_REF}"],"Architecture":"arm64"}}"#
    );
    let binary = fake_engine(temp.path(), true, &inspect, false);
    let run_dir = temp.path().join("architecture-abort");
    let error = ensure_image_with_adapter(
        OciImageAdapter::with_binary(SandboxEngine::Docker, binary),
        SandboxEngine::Docker,
        IMAGE_REF,
        OciPullPolicy::IfMissing,
        &host(),
        &run_dir,
        "architecture-abort-run",
    )
    .await
    .expect_err("architecture mismatch should abort");
    assert!(matches!(
        error,
        CaduceusError::OciImageArchitectureMismatch { .. }
    ));

    let record = read_record(&run_dir);
    assert_common_fields(&record);
    assert_eq!(record["resolved_id"], Value::Null);
    assert_eq!(record["repo_digests"], Value::Null);
    assert_eq!(record["architecture"], Value::Null);
    assert_eq!(record["variant"], Value::Null);
    assert_eq!(record["stage"], "arch_verify");
    assert_eq!(record["outcome"], "aborted");
    assert_eq!(record["error"]["variant"], "OciImageArchitectureMismatch");
    assert_eq!(record["verification"]["status"], "failed");
}

#[tokio::test]
async fn provenance_write_failure_does_not_fail_image_acquisition() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inspect = format!(
        r#"{{"Id":"sha256:{DIGEST}","RepoDigests":["{IMAGE_REF}"],"Architecture":"amd64"}}"#
    );
    let binary = fake_engine(temp.path(), true, &inspect, false);
    let run_path = temp.path().join("run-file");
    std::fs::write(&run_path, "not a directory").expect("run path fixture");

    let image = ensure_image_with_adapter(
        OciImageAdapter::with_binary(SandboxEngine::Docker, binary),
        SandboxEngine::Docker,
        IMAGE_REF,
        OciPullPolicy::IfMissing,
        &host(),
        &run_path,
        "provenance-write-failure",
    )
    .await
    .expect("provenance failure must not fail acquisition");

    assert_eq!(image.id, format!("sha256:{DIGEST}"));
}

#[tokio::test]
async fn pull_abort_is_audited_at_the_pull_stage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = fake_engine(temp.path(), false, "{}", true);
    let run_dir = temp.path().join("pull-abort");
    let error = ensure_image_with_adapter(
        OciImageAdapter::with_binary(SandboxEngine::Docker, binary),
        SandboxEngine::Docker,
        IMAGE_REF,
        OciPullPolicy::IfMissing,
        &host(),
        &run_dir,
        "pull-abort-run",
    )
    .await
    .expect_err("pull failure should abort");
    assert!(matches!(error, CaduceusError::OciPullFailed { .. }));

    let record = read_record(&run_dir);
    assert_common_fields(&record);
    assert_eq!(record["resolved_id"], Value::Null);
    assert_eq!(record["stage"], "pull");
    assert_eq!(record["outcome"], "aborted");
    assert_eq!(record["error"]["variant"], "OciPullFailed");
    assert_eq!(
        record["error"]["detail"]
            .as_str()
            .map(|s| s.contains("registry offline")),
        Some(true)
    );
    assert_eq!(record["pull_attempted"], true);
    assert_eq!(record["mode"], "pulled");
}
