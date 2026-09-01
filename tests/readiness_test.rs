use caduceus::config::Config;
use caduceus::executor::oci_platform::host_platform;
use caduceus::readiness::{
    assemble_report, render_human, run_live_with_options, CheckId, CheckResult, CheckStatus,
    DiagnosticCanary, DiagnosticStatus, ProbeOptions, ReadinessVerdict,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn checks(status: CheckStatus) -> Vec<CheckResult> {
    CheckId::ALL
        .into_iter()
        .map(|id| CheckResult {
            id,
            status,
            detail: "fixture".to_string(),
            remediation: None,
        })
        .collect()
}

#[test]
fn mandatory_verdict_matrix_is_fail_closed() {
    assert_eq!(
        assemble_report(checks(CheckStatus::Pass)).verdict,
        ReadinessVerdict::Ready
    );
    for failed_index in 0..CheckId::ALL.len() {
        let mut failed = checks(CheckStatus::Pass);
        failed[failed_index].status = CheckStatus::Fail;
        assert_eq!(
            assemble_report(failed).verdict,
            ReadinessVerdict::Unavailable,
            "check index {failed_index} must be mandatory"
        );
    }
}

#[test]
fn diagnostic_canary_cannot_change_mandatory_verdict() {
    let baseline = assemble_report(checks(CheckStatus::Pass));
    for status in [
        DiagnosticStatus::Pass,
        DiagnosticStatus::Skip,
        DiagnosticStatus::Failure,
    ] {
        let mut report = baseline.clone();
        report.diagnostic_canary = Some(DiagnosticCanary {
            status,
            detail: "fixture".to_string(),
        });
        assert_eq!(report.verdict, baseline.verdict);
        assert_eq!(report.checks, baseline.checks);
    }
}

#[test]
fn json_shape_keeps_readiness_and_diagnostic_sections_distinct() {
    let mut report = assemble_report(checks(CheckStatus::Pass));
    report.diagnostic_canary = Some(DiagnosticCanary {
        status: DiagnosticStatus::Skip,
        detail: "not configured".to_string(),
    });
    let value = serde_json::to_value(report).expect("serialize report");
    assert_eq!(value["verdict"], "READY");
    assert!(value["checks"].is_array());
    assert_eq!(value["diagnostic_canary"]["status"], "SKIP");
    assert!(value.get("verified_image").is_none());
}

#[test]
fn human_render_keeps_mandatory_and_diagnostic_results_distinct() {
    let mut report = assemble_report(checks(CheckStatus::Pass));
    report.diagnostic_canary = Some(DiagnosticCanary {
        status: DiagnosticStatus::Failure,
        detail: "fixture failure".to_string(),
    });
    let rendered = render_human(&report);
    assert!(rendered.contains("readiness: READY"));
    assert!(rendered.contains("diagnostic canary: FAILURE: fixture failure"));
}

#[cfg(unix)]
fn fake_engine(dir: &Path, image_present: bool, userns_remap: bool) -> PathBuf {
    let image_id = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let host = host_platform();
    let security_options = if userns_remap {
        "[name=userns-remap]"
    } else {
        "[]"
    };
    let image = if image_present {
        format!(
            "{{\"Id\":\"{image_id}\",\"RepoDigests\":[\"placeholder/worker@{image_id}\"],\"Architecture\":\"{}\"}}",
            host.architecture
        )
    } else {
        String::new()
    };
    let script = format!(
        "#!/bin/sh\ncase \"$1:$2:$3\" in\n  info:--format:*)\n    case \"$3\" in\n      *SecurityOptions*) printf '%s\\n' '{security_options}' ;;\n      *) printf '%s\\n' '{{\"ServerVersion\":\"test\",\"CgroupVersion\":\"2\",\"CgroupDriver\":\"systemd\"}}' ;;\n    esac\n    ;;\n  network:inspect:bridge) exit 0 ;;\n  image:inspect:*)\n    if [ \"$2\" = inspect ] && [ \"$3\" != --format ]; then\n      __PRESENT__\n    else\n      printf '%s\\n' '__IMAGE__'\n    fi\n    ;;\n  pull:*) exit 0 ;;\n  *) exit 1 ;;\nesac\n"
    )
    .replace(
        "__PRESENT__",
        if image_present { "exit 0" } else { "exit 1" },
    )
    .replace("__IMAGE__", &image);
    let path = dir.join("fake-engine");
    std::fs::write(&path, script).expect("write fake engine");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make fake engine executable");
    path
}

#[cfg(unix)]
fn ready_fixture(image_present: bool, userns_remap: bool) -> (TempDir, Config, ProbeOptions) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::test_defaults(dir.path());
    for path in [
        &config.state_dir,
        &config.repo_storage_root,
        &config.workdir_base,
    ] {
        std::fs::create_dir_all(path).expect("create config path");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("secure config path");
    }
    let cgroup_root = dir.path().join("cgroup");
    std::fs::create_dir_all(&cgroup_root).expect("create cgroup root");
    std::fs::write(cgroup_root.join("cgroup.controllers"), "cpu memory pids\n")
        .expect("write controllers");
    let engine = fake_engine(dir.path(), image_present, userns_remap);
    let options = ProbeOptions {
        cgroup_root,
        engine_binary: Some(engine),
    };
    config.sandbox.as_mut().expect("test sandbox").network = caduceus::config::SandboxNetwork::None;
    (dir, config, options)
}

#[cfg(unix)]
#[tokio::test]
async fn live_probe_reports_namespace_failure_without_running_dispatch() {
    let (_dir, config, options) = ready_fixture(true, true);
    let report = run_live_with_options(&config, &options).await;
    assert_eq!(report.verdict, ReadinessVerdict::Unavailable);
    let namespace = report
        .checks
        .iter()
        .find(|check| check.id == CheckId::Namespace)
        .expect("namespace check");
    assert_eq!(namespace.status, CheckStatus::Fail);
    assert!(namespace.remediation.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn live_probe_reports_missing_image_as_mandatory_failure() {
    let (_dir, config, options) = ready_fixture(false, false);
    let report = run_live_with_options(&config, &options).await;
    assert_eq!(report.verdict, ReadinessVerdict::Unavailable);
    assert_eq!(
        report
            .checks
            .iter()
            .find(|check| check.id == CheckId::Image)
            .expect("image check")
            .status,
        CheckStatus::Fail
    );
}

// The live probe's Platform check deliberately fails on non-Linux hosts
// (OCI workers require Linux), so the compliant-engine happy path can
// only reach ReadinessVerdict::Ready on Linux. Gate to target_os to keep
// the macOS CI gate green.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn live_probe_accepts_a_compliant_fake_engine() {
    let (_dir, config, options) = ready_fixture(true, false);
    let report = run_live_with_options(&config, &options).await;
    assert_eq!(
        report.verdict,
        ReadinessVerdict::Ready,
        "checks: {:?}",
        report.checks
    );
    assert!(report.verified_image.is_some());
    assert!(report
        .checks
        .iter()
        .all(|check| check.status == CheckStatus::Pass));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn live_probe_allows_nonwritable_group_access_on_state_and_worktree() {
    let (_dir, config, options) = ready_fixture(true, false);
    for path in [&config.state_dir, &config.workdir_base] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o750))
            .expect("relax non-repository directory mode");
    }
    let report = run_live_with_options(&config, &options).await;
    assert_eq!(
        report.verdict,
        ReadinessVerdict::Ready,
        "checks: {:?}",
        report.checks
    );
}

#[cfg(unix)]
#[tokio::test]
async fn live_probe_requires_private_repository_storage_root() {
    let (_dir, config, options) = ready_fixture(true, false);
    std::fs::set_permissions(
        &config.repo_storage_root,
        std::fs::Permissions::from_mode(0o750),
    )
    .expect("relax repository storage mode");
    let report = run_live_with_options(&config, &options).await;
    let filesystem = report
        .checks
        .iter()
        .find(|check| check.id == CheckId::Filesystem)
        .expect("filesystem check");
    assert_eq!(filesystem.status, CheckStatus::Fail);
    assert!(filesystem.detail.contains("expected 700"));
}

#[cfg(unix)]
#[tokio::test]
async fn live_probe_reports_reserve_breach_as_filesystem_failure() {
    let (_dir, mut config, options) = ready_fixture(true, false);
    config
        .sandbox
        .as_mut()
        .expect("test sandbox")
        .reserved_host_disk_mb = u64::MAX;
    let report = run_live_with_options(&config, &options).await;
    assert_eq!(report.verdict, ReadinessVerdict::Unavailable);
    let filesystem = report
        .checks
        .iter()
        .find(|check| check.id == CheckId::Filesystem)
        .expect("filesystem check");
    assert_eq!(filesystem.status, CheckStatus::Fail);
    assert!(filesystem
        .remediation
        .as_deref()
        .is_some_and(|hint| !hint.is_empty()));
}

#[cfg(unix)]
#[tokio::test]
async fn live_probe_reports_missing_cgroup_controller() {
    let (dir, config, options) = ready_fixture(true, false);
    std::fs::write(
        dir.path().join("cgroup").join("cgroup.controllers"),
        "cpu memory\n",
    )
    .expect("remove pids controller");
    let report = run_live_with_options(&config, &options).await;
    assert_eq!(report.verdict, ReadinessVerdict::Unavailable);
    let resources = report
        .checks
        .iter()
        .find(|check| check.id == CheckId::Resources)
        .expect("resources check");
    assert_eq!(resources.status, CheckStatus::Fail);
    assert!(resources.detail.contains("pids"));
}
