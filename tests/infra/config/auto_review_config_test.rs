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

// --- Deprecation warning (DAR §12) + no silent translation (DAR §5) ---
//
// SERIALIZATION RULE (#167 tracing-callsite-interest trap): any test
// that executes the deprecation-warn callsite with NO subscriber
// installed can poison the callsite's cached interest as
// never-enabled, silently dropping the event for later capture tests.
// Every test whose config sets `ticket_label_investigation` — even
// the non-capture ones — must run #[serial_test::serial] so no
// sibling executes the callsite concurrently unhooked. Capture tests
// additionally run their load inside `init_for_test` so every
// callsite execution in this binary happens under an active
// subscriber.
//
// The capture assertions check the JSON line level is WARN (the
// test subscriber runs at TRACE, so a warn line is present; the
// level tag proves it is a warning, not an info line).

#[test]
#[serial_test::serial]
fn explicit_investigation_label_emits_deprecation_warn() {
    use caduceus::logging::init_for_test;
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("warn.log");
    let body = format!(
        "{}ticket_label_investigation: \"autofix-investigate\"\n",
        trusted_host_base()
    );
    let path = dir.path().join("config.yaml");
    let body = body.replace("__TMP__", &dir.path().to_string_lossy());
    std::fs::write(&path, body).expect("write config");
    init_for_test(&log, || {
        Config::load_from(&path).expect("config loads (warning only)")
    })
    .expect("capture body");
    let logged = std::fs::read_to_string(&log).expect("read log");
    assert!(
        logged.contains("ticket_label_investigation is deprecated"),
        "got: {logged}"
    );
    assert!(logged.contains("deprecated"), "got: {logged}");
    assert!(
        logged.contains("\"level\":\"WARN\""),
        "must be WARN level, got: {logged}"
    );
}

#[test]
#[serial_test::serial]
fn default_investigation_label_does_not_warn() {
    use caduceus::logging::init_for_test;
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("silent.log");
    let body = trusted_host_base().replace("__TMP__", &dir.path().to_string_lossy());
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, body).expect("write config");
    init_for_test(&log, || Config::load_from(&path).expect("config loads")).expect("capture body");
    let logged = std::fs::read_to_string(&log).expect("read log");
    assert!(
        !logged.contains("ticket_label_investigation is deprecated"),
        "default must not warn, got: {logged}"
    );
    // The resolved label is still the transitional default.
    // (Re-load outside the capture to assert the value.)
    let cfg = Config::load_from(&path).expect("reload");
    assert_eq!(cfg.ticket_label_investigation, "autofix-investigate");
}

#[test]
#[serial_test::serial]
fn legacy_emoji_investigation_label_still_translates_and_now_also_warns_deprecation() {
    // An explicitly-set legacy emoji value hits BOTH warns: the
    // #291 translation warn and the #320 deprecation warn.
    use caduceus::logging::init_for_test;
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("both.log");
    let body = format!(
        "{}ticket_label_investigation: \"🤖 auto-fix-investigate\"\n",
        trusted_host_base()
    )
    .replace("__TMP__", &dir.path().to_string_lossy());
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, body).expect("write config");
    init_for_test(&log, || {
        Config::load_from(&path).expect("config loads (warning only)")
    })
    .expect("capture body");
    let logged = std::fs::read_to_string(&log).expect("read log");
    assert!(
        logged.contains("translated at read time"),
        "#291 translation warn must fire, got: {logged}"
    );
    assert!(
        logged.contains("ticket_label_investigation is deprecated"),
        "deprecation warn must fire too, got: {logged}"
    );
    let cfg = Config::load_from(&path).expect("reload");
    assert_eq!(cfg.ticket_label_investigation, "autofix-investigate");
}

#[test]
#[serial_test::serial]
fn investigation_config_never_feeds_auto_review() {
    // AC5: explicit investigation config, no auto_review block ⇒
    // cfg.auto_review is None. Investigation config has zero effect on
    // the review block (DAR §12: never mapped). The load runs inside
    // `init_for_test` so this callsite execution never poisons the
    // interest cache for the sibling capture tests (#167).
    use caduceus::logging::init_for_test;
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("no-translate.log");
    let body = format!(
        "{}ticket_label_investigation: \"autofix-investigate\"\n",
        trusted_host_base()
    )
    .replace("__TMP__", &dir.path().to_string_lossy());
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, body).expect("write config");
    let cfg = init_for_test(&log, || {
        Config::load_from(&path).expect("loads with warning only")
    })
    .expect("capture body");
    assert!(cfg.auto_review.is_none());
}
