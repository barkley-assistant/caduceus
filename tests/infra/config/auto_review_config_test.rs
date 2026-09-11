//! Config-loader tests for the `auto_review:` block, the
//! OCI-required validation (DAR §6.3), `max_reviews_per_tick`, and
//! the `ticket_label_investigation` removal (issue #382, DAR §12).
//! Mirrors sandbox_config_test.rs: load through the
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

// --- OCI-required validation matrix (DAR §6.3) ---

#[test]
fn matrix_absent_block_is_fine_on_trusted_host() {
    let cfg = load(&trusted_host_base()).expect("no block = disabled");
    assert!(cfg.auto_review.is_none());
}

#[test]
fn matrix_enabled_false_block_is_inert_on_trusted_host() {
    let cfg = load(&format!(
        "{}auto_review:\n  enabled: false\n",
        trusted_host_base()
    ))
    .expect("enabled: false = explicit no-op");
    let ar = cfg.auto_review().expect("block present");
    assert!(!ar.enabled);
    assert!(!ar.draft_pull_requests);
}

#[test]
fn matrix_trusted_host_error_survives_present_valid_sandbox() {
    // Cell 3: a sandbox: section does NOT soften the TrustedHost
    // refusal — DAR §6.3 requires executor_mode to BE oci.
    let err = load(&format!(
        "{}executor_mode: trusted_host\nsandbox:\n  image: \"{VALID_IMAGE}\"\n\
         auto_review:\n  enabled: true\n",
        trusted_host_base()
    ))
    .expect_err("sandbox presence must not bypass the oci requirement");
    let msg = format!("{err}");
    assert!(msg.contains("auto_review.enabled"), "got: {msg}");
    assert!(msg.contains("executor_mode: oci"), "got: {msg}");
}

#[test]
fn matrix_enabled_oci_missing_sandbox_errors_from_existing_check() {
    let err = load(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"__TMP__/state\"\n\
         executor_mode: oci\n\
         auto_review:\n  enabled: true\n",
    )
    .expect_err("oci without sandbox must fail (existing rule)");
    let msg = format!("{err}");
    assert!(
        msg.contains("executor_mode 'oci' requires a `sandbox:` section"),
        "got: {msg}"
    );
}

#[test]
fn matrix_enabled_oci_valid_sandbox_is_the_ok_shape() {
    let cfg = load(&format!(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"__TMP__/state\"\n\
         executor_mode: oci\n\
         sandbox:\n  image: \"{VALID_IMAGE}\"\n\
         auto_review:\n  enabled: true\n  draft_pull_requests: true\n"
    ))
    .expect("enabled + oci + valid sandbox loads");
    let ar = cfg.auto_review().expect("block resolved");
    assert!(ar.enabled);
    assert!(ar.draft_pull_requests);
}

#[test]
fn matrix_enabled_oci_invalid_image_errors_from_sandbox_validation() {
    let err = load(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"__TMP__/state\"\n\
         executor_mode: oci\n\
         sandbox:\n  image: \"not-a-digest\"\n\
         auto_review:\n  enabled: true\n",
    )
    .expect_err("bad image must fail (existing rule)");
    let msg = format!("{err}");
    assert!(msg.contains("sandbox.image"), "got: {msg}");
}

#[test]
fn unknown_auto_review_key_is_rejected() {
    let err = load(&format!(
        "{}auto_review:\n  enabled: true\n  minimum_severity: warning\n",
        trusted_host_base()
    ));
    assert!(err.is_err(), "Phase-2 keys must fail at parse time");
}

// --- `ticket_label_investigation` removal (issue #382) ---
//
// The legacy key was removed; a config carrying it fails at parse time
// with serde's generic unknown-field error — no special-casing, no
// guided message.

#[test]
fn ticket_label_investigation_fails_at_parse_as_unknown_field() {
    let err = load(&format!(
        "{}ticket_label_investigation: \"autofix-investigate\"\n",
        trusted_host_base()
    ))
    .expect_err("the removed key must fail at parse time");
    let msg = format!("{err}");
    assert!(
        msg.contains("unknown field"),
        "expected serde unknown-field error, got: {msg}"
    );
    assert!(
        msg.contains("ticket_label_investigation"),
        "error must name the unknown field: {msg}"
    );
}

#[test]
fn ticket_label_investigation_absent_loads_cleanly() {
    let cfg = load(&trusted_host_base()).expect("key absent = clean load");
    assert!(cfg.auto_review.is_none());
    assert_eq!(cfg.ticket_label_code, "autofix");
}

#[test]
fn investigation_config_never_feeds_auto_review() {
    // The key is rejected, so a clean load can never have carried
    // investigation config into the review block. Pin the inverse:
    // an explicit auto_review block is the only way to enable it.
    let cfg = load(&format!(
        "{}auto_review:\n  enabled: false\n",
        trusted_host_base()
    ))
    .expect("clean load");
    assert!(cfg.auto_review.is_some());
    assert!(!cfg.auto_review().expect("block").enabled);
}

// --- `rerun_command` (DAR §17, issue #335) ---

#[test]
fn rerun_command_defaults_to_caduceus_review() {
    let cfg = load(&format!(
        "{}auto_review:\n  enabled: false\n",
        trusted_host_base()
    ))
    .expect("config loads");
    let ar = cfg.auto_review().expect("block present");
    assert_eq!(ar.rerun_command, "/caduceus review");
}

#[test]
fn rerun_command_empty_rejected() {
    let err = load(&format!(
        "{}auto_review:\n  rerun_command: \"\"\n",
        trusted_host_base()
    ))
    .expect_err("empty rerun_command must fail config load");
    let msg = format!("{err}");
    assert!(msg.contains("rerun_command"), "got: {msg}");
    assert!(msg.contains("must not be empty"), "got: {msg}");
}

#[test]
fn rerun_command_without_slash_rejected() {
    let err = load(&format!(
        "{}auto_review:\n  rerun_command: \"review\"\n",
        trusted_host_base()
    ))
    .expect_err("non-slash rerun_command must fail config load");
    let msg = format!("{err}");
    assert!(msg.contains("rerun_command"), "got: {msg}");
    assert!(msg.contains("must start with '/'"), "got: {msg}");
}

#[test]
fn rerun_command_custom_accepted() {
    let cfg = load(&format!(
        "{}auto_review:\n  rerun_command: \"/rerun\"\n",
        trusted_host_base()
    ))
    .expect("custom command loads");
    let ar = cfg.auto_review().expect("block present");
    assert_eq!(ar.rerun_command, "/rerun");
}

#[test]
fn rerun_command_invalid_plus_enabled_trusted_host_names_both_errors() {
    // Validation errors join the shared `errors` vec, so a bad command
    // AND a TrustedHost+enabled config fail together with both named.
    let err = load(&format!(
        "{}auto_review:\n  enabled: true\n  rerun_command: \"review\"\n",
        trusted_host_base()
    ))
    .expect_err("both conditions must fail together");
    let msg = format!("{err}");
    assert!(msg.contains("rerun_command"), "got: {msg}");
    assert!(msg.contains("auto_review.enabled"), "got: {msg}");
}
