//! Matrix for `auto_review.publication_mode` (issue #394):
//! absent => `update` (zero behavior change); both values accepted;
//! unknown value => hard error naming the field (the #380 lesson).

use caduceus::config::{Config, PublicationMode};
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

fn config_body(mode_line: &str) -> String {
    format!(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"__TMP__/state\"\n\
         reduced_containment_acknowledged: true\n\
         watched_repos: [\"owner/repo\"]\n\
         executor_mode: oci\n\
         sandbox:\n\
         \x20 image: \"{VALID_IMAGE}\"\n\
         auto_review:\n\
         \x20 enabled: true\n{mode_line}"
    )
}

fn assert_rejected_with(body: &str, expected_fragment: &str) {
    let err = load(body).expect_err("config must be rejected");
    match &err {
        CaduceusError::Config(msg) => assert!(
            msg.contains(expected_fragment),
            "error {msg:?} must mention {expected_fragment:?}"
        ),
        other => panic!("expected CaduceusError::Config; got: {other:?}"),
    }
}

#[test]
fn absent_publication_mode_defaults_to_update() {
    let cfg = load(&config_body("")).expect("config loads");
    let ar = cfg.auto_review().expect("auto_review block present");
    assert_eq!(ar.publication_mode, PublicationMode::Update);
}

#[test]
fn explicit_update_value_resolves() {
    let cfg = load(&config_body("  publication_mode: update\n")).expect("config loads");
    let ar = cfg.auto_review().expect("auto_review block present");
    assert_eq!(ar.publication_mode, PublicationMode::Update);
}

#[test]
fn new_comment_value_resolves() {
    let cfg = load(&config_body("  publication_mode: new_comment\n")).expect("config loads");
    let ar = cfg.auto_review().expect("auto_review block present");
    assert_eq!(ar.publication_mode, PublicationMode::NewComment);
}

#[test]
fn unknown_value_is_a_hard_error_naming_the_field() {
    assert_rejected_with(
        &config_body("  publication_mode: banana\n"),
        "auto_review.publication_mode",
    );
    assert_rejected_with(&config_body("  publication_mode: banana\n"), "banana");
}
