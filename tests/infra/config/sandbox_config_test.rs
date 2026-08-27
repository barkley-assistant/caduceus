//! Unit and regression tests for the authoritative `sandbox:` config
//! section (sandbox-config spec scenarios).
//!
//! Two error channels are exercised:
//!
//! * **serde** (YAML-parse time) — unknown enum values (`engine`,
//!   `network`, `pull_policy`), unknown fields (top-level removed
//!   prototype keys, unknown keys inside `sandbox:`), and negative u64
//!   resources. These surface as `CaduceusError::Yaml` carrying the
//!   serde message.
//! * **`Config::from_raw`** — range validations (image regex, resource
//!   floors, zero timeouts, `cpus` non-finite). These surface as
//!   `CaduceusError::Config` with the aggregated messages.
//!
//! Tests assert on message *content*, never on the `CaduceusError`
//! variant, for the serde-side cases (serde_yaml 0.9 does not embed
//! the field path in nested errors).

use caduceus::executor::oci_args::SandboxEngine;
use caduceus::infra::config::{
    Config, OciPullPolicy, SandboxConfig, SandboxNetwork, SandboxResources,
};
use caduceus::infra::error::CaduceusError;

const VALID_IMAGE: &str =
    "caduceus-worker@sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Write a standalone config body (with `__TMP__` expanded to the
/// tempdir path) and load it through the canonical `Config::load_from`
/// chain: YAML → `RawConfig` → `Config::from_raw`.
fn load(body: &str) -> Result<Config, CaduceusError> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    let body = body.replace("__TMP__", &dir.path().to_string_lossy());
    std::fs::write(&path, body).expect("write config");
    Config::load_from(&path)
}

/// Body prefix shared by every test: a valid TrustedHost base plus a
/// `sandbox:` section with the given body.
fn with_sandbox(sandbox_body: &str) -> String {
    format!(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"__TMP__/state\"\n\
         reduced_containment_acknowledged: true\n\
         {sandbox_body}"
    )
}

fn trusted_host_base() -> &'static str {
    "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
     state_dir: \"__TMP__/state\"\n\
     reduced_containment_acknowledged: true\n"
}

// ---------------------------------------------------------------------------
// 10.1 — from_raw success paths (spec: defaulted and explicit full parse)
// ---------------------------------------------------------------------------

#[test]
fn defaulted_sandbox_parses_with_table_defaults() {
    let cfg = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n"
    )))
    .expect("defaulted sandbox must load");
    let sb = cfg.sandbox();
    assert_eq!(sb.engine, SandboxEngine::Docker);
    assert_eq!(sb.pull_policy, OciPullPolicy::IfMissing);
    assert_eq!(sb.resources.cpus, 2.0);
    assert_eq!(sb.resources.memory_mb, 2048);
    assert_eq!(sb.resources.pids, 256);
    assert_eq!(sb.resources.tmpfs_mb, 256);
    assert_eq!(sb.resources.shm_mb, 64);
    assert_eq!(sb.network, SandboxNetwork::None);
    assert!(sb.pass_env.is_empty());
    assert_eq!(sb.stop_timeout_seconds, 10);
    assert_eq!(sb.kill_timeout_seconds, 5);
    assert_eq!(sb.reconcile_timeout_seconds, 60);
    assert_eq!(sb.reserved_host_disk_mb, 2048);
}

#[test]
fn explicit_full_sandbox_parses_to_given_values() {
    let cfg = load(&with_sandbox(
        "executor_mode: oci\n\
         sandbox:\n\
         \x20 engine: podman\n\
         \x20 image: \"caduceus-worker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n\
         \x20 pull_policy: always\n\
         \x20 resources: { cpus: 4.0, memory_mb: 4096, pids: 512, tmpfs_mb: 512, shm_mb: 128 }\n\
         \x20 network: unrestricted\n\
         \x20 pass_env: [\"HTTP_PROXY\", \"NO_PROXY\"]\n\
         \x20 stop_timeout_seconds: 30\n\
         \x20 kill_timeout_seconds: 15\n\
         \x20 reconcile_timeout_seconds: 120\n\
         \x20 reserved_host_disk_mb: 8192\n",
    ))
    .expect("explicit full sandbox must load");
    let sb = cfg.sandbox();
    assert_eq!(sb.engine, SandboxEngine::Podman);
    assert_eq!(
        sb.image,
        "caduceus-worker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(sb.pull_policy, OciPullPolicy::Always);
    assert_eq!(sb.resources.cpus, 4.0);
    assert_eq!(sb.resources.memory_mb, 4096);
    assert_eq!(sb.resources.pids, 512);
    assert_eq!(sb.resources.tmpfs_mb, 512);
    assert_eq!(sb.resources.shm_mb, 128);
    assert_eq!(sb.network, SandboxNetwork::Unrestricted);
    assert_eq!(
        sb.pass_env,
        vec!["HTTP_PROXY".to_string(), "NO_PROXY".to_string()]
    );
    assert_eq!(sb.stop_timeout_seconds, 30);
    assert_eq!(sb.kill_timeout_seconds, 15);
    assert_eq!(sb.reconcile_timeout_seconds, 120);
    assert_eq!(sb.reserved_host_disk_mb, 8192);
}

// ---------------------------------------------------------------------------
// 10.2 — sandbox.image rejections (spec scenarios)
// ---------------------------------------------------------------------------

fn assert_image_rejected(image: &str, expected_fragment: &str) {
    let err = load(&with_sandbox(&format!("sandbox:\n  image: {image}\n")))
        .expect_err("invalid sandbox.image must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.image"),
        "error must name sandbox.image; got: {msg}"
    );
    assert!(
        msg.contains(expected_fragment),
        "error must contain {expected_fragment:?}; got: {msg}"
    );
}

#[test]
fn tag_only_image_reference_rejected() {
    assert_image_rejected("\"caduceus-worker:latest\"", "name@sha256:<64 hex>");
}

#[test]
fn empty_image_reference_rejected() {
    assert_image_rejected("\"\"", "must not be empty");
}

#[test]
fn non_sha256_digest_rejected() {
    assert_image_rejected(
        "\"caduceus-worker@sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
        "must use the sha256 algorithm",
    );
}

#[test]
fn short_digest_rejected() {
    assert_image_rejected(
        "\"caduceus-worker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
        "must be exactly 64 hex chars",
    );
}

#[test]
fn overlong_digest_rejected() {
    let long = "a".repeat(128);
    assert_image_rejected(
        &format!("\"caduceus-worker@sha256:{long}\""),
        "must be exactly 64 hex chars",
    );
}

// ---------------------------------------------------------------------------
// 10.3 — serde enum rejections (surface from serde, not from_raw)
// ---------------------------------------------------------------------------

fn load_sandbox_line(key_value: &str) -> Result<Config, CaduceusError> {
    load(&with_sandbox(&format!("sandbox:\n  {key_value}\n")))
}

#[test]
fn unknown_engine_value_rejected() {
    let err = load_sandbox_line(&format!(
        "image: \"{VALID_IMAGE}\"\n  engine: \"containerd\""
    ))
    .expect_err("unknown engine must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("unknown variant"), "got: {msg}");
    assert!(msg.contains("containerd"), "got: {msg}");
    assert!(
        msg.contains("docker") && msg.contains("podman"),
        "allowed values must be listed; got: {msg}"
    );
}

#[test]
fn unknown_network_value_rejected() {
    let err = load_sandbox_line(&format!(
        "image: \"{VALID_IMAGE}\"\n  network: \"filtered\""
    ))
    .expect_err("unknown network must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("unknown variant"), "got: {msg}");
    assert!(msg.contains("filtered"), "got: {msg}");
    assert!(
        msg.contains("none") && msg.contains("unrestricted"),
        "allowed values must be listed; got: {msg}"
    );
}

#[test]
fn unknown_pull_policy_value_rejected() {
    let err = load_sandbox_line(&format!(
        "image: \"{VALID_IMAGE}\"\n  pull_policy: \"missing\""
    ))
    .expect_err("unknown pull_policy must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("unknown variant"), "got: {msg}");
    assert!(
        msg.contains("never") && msg.contains("if_missing") && msg.contains("always"),
        "allowed values must be listed; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 10.4 — from_raw range rejections + zero-floor successes
// ---------------------------------------------------------------------------

#[test]
fn zero_stop_timeout_rejected() {
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  stop_timeout_seconds: 0\n"
    )))
    .expect_err("zero stop timeout must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.stop_timeout_seconds") && msg.contains("> 0"),
        "got: {msg}"
    );
}

#[test]
fn zero_kill_timeout_rejected() {
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  kill_timeout_seconds: 0\n"
    )))
    .expect_err("zero kill timeout must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.kill_timeout_seconds") && msg.contains("> 0"),
        "got: {msg}"
    );
}

#[test]
fn zero_reconcile_timeout_rejected() {
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  reconcile_timeout_seconds: 0\n"
    )))
    .expect_err("zero reconcile timeout must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.reconcile_timeout_seconds") && msg.contains("> 0"),
        "got: {msg}"
    );
}

#[test]
fn cpus_below_floor_rejected() {
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  resources: {{ cpus: 0.1 }}\n"
    )))
    .expect_err("cpus below 0.25 floor must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.resources.cpus") && msg.contains(">= 0.25"),
        "got: {msg}"
    );
}

#[test]
fn cpus_non_finite_rejected() {
    // YAML `.nan` parses to f64 NaN, which would slip a `<` comparison
    // — the non-finite check must reject it before the floor check.
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  resources: {{ cpus: .nan }}\n"
    )))
    .expect_err("non-finite cpus must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.resources.cpus") && msg.contains(">= 0.25"),
        "got: {msg}"
    );
}

#[test]
fn memory_mb_below_floor_rejected() {
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  resources: {{ memory_mb: 32 }}\n"
    )))
    .expect_err("memory_mb below 64 floor must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.resources.memory_mb") && msg.contains(">= 64"),
        "got: {msg}"
    );
}

#[test]
fn pids_below_floor_rejected() {
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  resources: {{ pids: 8 }}\n"
    )))
    .expect_err("pids below 16 floor must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox.resources.pids") && msg.contains(">= 16"),
        "got: {msg}"
    );
}

#[test]
fn tmpfs_mb_valid_at_zero() {
    let cfg = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  resources: {{ tmpfs_mb: 0 }}\n"
    )))
    .expect("tmpfs_mb 0 is a valid floor");
    assert_eq!(cfg.sandbox().resources.tmpfs_mb, 0);
}

#[test]
fn shm_mb_valid_at_zero() {
    let cfg = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  resources: {{ shm_mb: 0 }}\n"
    )))
    .expect("shm_mb 0 is a valid floor");
    assert_eq!(cfg.sandbox().resources.shm_mb, 0);
}

// ---------------------------------------------------------------------------
// 10.5 — nested deny_unknown_fields
// ---------------------------------------------------------------------------

#[test]
fn unknown_key_inside_sandbox_rejected() {
    let err = load(&with_sandbox(&format!(
        "sandbox:\n  image: \"{VALID_IMAGE}\"\n  bogus: 1\n"
    )))
    .expect_err("unknown key inside sandbox must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("unknown field"), "got: {msg}");
    assert!(msg.contains("bogus"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// 10.6 — obsolete prototype fields rejected (serde unknown-field errors)
// ---------------------------------------------------------------------------

#[test]
fn legacy_oci_image_digest_rejected() {
    let err = load(&format!(
        "{}oci_image_digest: \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n",
        trusted_host_base()
    ))
    .expect_err("legacy oci_image_digest must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("unknown field"), "got: {msg}");
    assert!(msg.contains("oci_image_digest"), "got: {msg}");
}

#[test]
fn legacy_network_profiles_rejected() {
    let err = load(&format!(
        "{}network_profiles:\n  isolated:\n    egress_allow: [\"10.0.0.0/8\"]\n",
        trusted_host_base()
    ))
    .expect_err("legacy network_profiles must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("unknown field"), "got: {msg}");
    assert!(msg.contains("network_profiles"), "got: {msg}");
}

#[test]
fn legacy_upgrade_choice_rejected() {
    let err = load(&format!("{}upgrade_choice: chosen\n", trusted_host_base()))
        .expect_err("legacy upgrade_choice must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("unknown field"), "got: {msg}");
    assert!(msg.contains("upgrade_choice"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// 10.7 — TrustedHost seam unchanged
// ---------------------------------------------------------------------------

#[test]
fn trusted_host_config_loads_byte_identically_without_sandbox() {
    let state_dir = "__TMP__/var/lib/caduceus";
    let cfg = load(&format!(
        "executor_mode: trusted_host\n\
         worker_command: [\"/usr/local/bin/worker\"]\n\
         reduced_containment_acknowledged: true\n\
         state_dir: \"{state_dir}\"\n"
    ))
    .expect("TrustedHost config without sandbox must load");
    assert_eq!(
        cfg.executor_mode,
        caduceus::executor::ExecutorKind::TrustedHost
    );
    assert_eq!(
        cfg.worker_command,
        vec!["/usr/local/bin/worker".to_string()]
    );
    assert!(cfg.reduced_containment_acknowledged);
    assert!(
        cfg.state_dir
            .to_string_lossy()
            .ends_with("var/lib/caduceus"),
        "state_dir must equal the supplied value, got: {}",
        cfg.state_dir.display()
    );
    assert!(
        cfg.sandbox.is_none(),
        "sandbox must be None for TrustedHost"
    );
}

// ---------------------------------------------------------------------------
// 10.8 — OCI mode without sandbox section
// ---------------------------------------------------------------------------

#[test]
fn oci_mode_missing_sandbox_rejected() {
    let err = load(&format!("{}executor_mode: oci\n", trusted_host_base()))
        .expect_err("OCI mode without sandbox: must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sandbox") && msg.contains("sandbox.image"),
        "error must name the missing sandbox section and image; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Support: prove the typed success path values for hand-built configs
// ---------------------------------------------------------------------------

#[test]
fn test_defaults_sandbox_is_some() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(tmp.path());
    let sb: &SandboxConfig = cfg.sandbox();
    assert_eq!(sb.engine, SandboxEngine::Docker);
    assert_eq!(sb.pull_policy, OciPullPolicy::IfMissing);
    assert_eq!(
        sb.image,
        format!("caduceus-worker@sha256:{}", "0".repeat(64))
    );
    let res: &SandboxResources = &sb.resources;
    assert_eq!(res.cpus, 2.0);
    assert_eq!(res.memory_mb, 2048);
    assert_eq!(res.pids, 256);
    assert_eq!(res.tmpfs_mb, 256);
    assert_eq!(res.shm_mb, 64);
    assert_eq!(sb.network, SandboxNetwork::None);
    assert_eq!(sb.stop_timeout_seconds, 10);
    assert_eq!(sb.kill_timeout_seconds, 5);
    assert_eq!(sb.reconcile_timeout_seconds, 60);
    assert_eq!(sb.reserved_host_disk_mb, 2048);
}
