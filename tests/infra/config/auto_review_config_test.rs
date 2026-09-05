//! Config-loader tests for the `auto_review:` block, the
//! OCI-required validation (DAR §6.3), `max_reviews_per_tick`, and
//! the release-N `ticket_label_investigation` deprecation warning
//! (DAR §12). Mirrors sandbox_config_test.rs: load through the
//! canonical `Config::load_from` chain, assert on message content.

use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;

const VALID_IMAGE: &str =
    "caduceus-worker@sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn load(body: &str) -> Result<Config, CaduceusError> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    let body = body.replace("__TMP__", &dir.path().to_string_lossy());
    std::fs::write(&path, body).expect("write config");
    Config::load_from(&path)
}

fn trusted_host_base() -> String {
    "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
     state_dir: \"__TMP__/state\"\n\
     reduced_containment_acknowledged: true\n"
        .to_string()
}

#[test]
fn auto_review_enabled_with_trusted_host_is_rejected() {
    let err = load(&format!(
        "{}auto_review:\n  enabled: true\n",
        trusted_host_base()
    ))
    .expect_err("enabled + trusted_host must fail");
    let msg = format!("{err}");
    assert!(msg.contains("auto_review.enabled"), "got: {msg}");
    assert!(msg.contains("executor_mode: oci"), "got: {msg}");
    assert!(msg.contains("sandbox:"), "got: {msg}");
    assert!(msg.contains("caduceus doctor"), "got: {msg}");
}
