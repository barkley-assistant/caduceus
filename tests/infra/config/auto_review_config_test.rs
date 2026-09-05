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

#[test]
fn max_reviews_per_tick_defaults_to_parallelism_x4() {
    let cfg =
        load(&format!("{}worker_parallelism: 3\n", trusted_host_base())).expect("config loads");
    assert_eq!(cfg.max_reviews_per_tick, 12);
}

#[test]
fn max_reviews_per_tick_explicit_value_wins() {
    let cfg = load(&format!(
        "{}worker_parallelism: 3\nmax_reviews_per_tick: 5\n",
        trusted_host_base()
    ))
    .expect("config loads");
    assert_eq!(cfg.max_reviews_per_tick, 5);
}

#[test]
fn max_reviews_per_tick_zero_is_unbounded_not_rejected() {
    let cfg = load(&format!("{}max_reviews_per_tick: 0\n", trusted_host_base()))
        .expect("0 = unbounded opt-in, mirrors max_issues_per_tick");
    assert_eq!(cfg.max_reviews_per_tick, 0);
}

#[test]
fn max_reviews_per_tick_saturates_instead_of_overflowing() {
    let cfg = load(&format!(
        "{}worker_parallelism: 4294967295\n",
        trusted_host_base()
    ))
    .expect("saturating_mul must not panic");
    assert_eq!(cfg.max_reviews_per_tick, u32::MAX);
}
