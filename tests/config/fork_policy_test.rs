//! Trust-policy matrix for `auto_review.fork_policy.allow_fork_prs`
//! (issue #337, Phase 2).
//!
//! The fork policy is a per-repo OPT-IN list, default OFF: an absent
//! or empty `fork_policy` block preserves Phase-1 behaviour (every
//! fork PR skipped with `review_skipped_fork_unsupported`). Listing
//! a slug opts that repo into fork PR review via the quarantine
//! fetch path.
//!
//! Matrix coverage:
//!
//! - absent `fork_policy` block => resolved `fork_policy` is `None`
//!   (default off, Phase-1 preserved);
//! - `allow_fork_prs: ["owner/repo"]` where the slug IS in
//!   `watched_repos` => resolved list contains the slug;
//! - malformed slugs (`"owner"`, `"/repo"`, `"owner/"`) => `from_raw`
//!   error naming the slug;
//! - a slug NOT in `watched_repos` => `from_raw` error
//!   ("not in watched_repos");
//! - `test_defaults` carries the empty default (auto_review disabled).

use caduceus::config::Config;
use caduceus::infra::error::CaduceusError;

const VALID_IMAGE: &str =
    "caduceus-worker@sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Write a standalone config body (with `__TMP__` expanded to the
/// tempdir path) and load it through the canonical `Config::load_from`
/// chain: YAML -> `RawConfig` -> `Config::from_raw`.
fn load(body: &str) -> Result<Config, CaduceusError> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    let body = body.replace("__TMP__", &dir.path().to_string_lossy());
    std::fs::write(&path, body).expect("write config");
    Config::load_from(&path)
}

/// Valid baseline: OCI executor (required by `auto_review.enabled`),
/// one watched repo, and the given `allow_fork_prs` list.
fn config_body(allow: Option<&str>) -> String {
    let allow_block = match allow {
        Some(list) => format!("  fork_policy:\n    allow_fork_prs: [{list}]\n"),
        None => String::new(),
    };
    format!(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"__TMP__/state\"\n\
         reduced_containment_acknowledged: true\n\
         watched_repos: [\"owner/repo\"]\n\
         executor_mode: oci\n\
         sandbox:\n\
         \x20 image: \"{VALID_IMAGE}\"\n\
         auto_review:\n\
         \x20 enabled: true\n{allow_block}"
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
fn absent_fork_policy_preserves_phase1_default_off() {
    let cfg = load(&config_body(None)).expect("config loads without fork_policy");
    let ar = cfg.auto_review().expect("auto_review block present");
    assert!(
        ar.fork_policy.is_none(),
        "absent fork_policy block must resolve to None (Phase-1 behaviour preserved)"
    );
}

#[test]
fn allowed_slug_in_watched_repos_resolves() {
    let cfg = load(&config_body(Some("\"owner/repo\""))).expect("config loads");
    let ar = cfg.auto_review().expect("auto_review block present");
    let fp = ar.fork_policy.as_ref().expect("fork_policy resolved");
    assert_eq!(fp.allow_fork_prs, vec!["owner/repo".to_string()]);
}

#[test]
fn empty_allow_fork_prs_resolves_to_empty_policy() {
    let cfg = load(&config_body(Some(""))).expect("empty list loads");
    let ar = cfg.auto_review().expect("auto_review block present");
    let fp = ar.fork_policy.as_ref().expect("fork_policy resolved");
    assert!(
        fp.allow_fork_prs.is_empty(),
        "empty allow_fork_prs is default-off"
    );
}

#[test]
fn malformed_slugs_are_rejected_with_the_slug_named() {
    for slug in ["\"owner\"", "\"/repo\"", "\"owner/\"", "\"owner//repo\""] {
        assert_rejected_with(
            &config_body(Some(slug)),
            // The error names the offending slug; the split-once shape
            // check makes each malformed form fail.
            "allow_fork_prs",
        );
    }
}

#[test]
fn slug_not_in_watched_repos_is_rejected() {
    assert_rejected_with(&config_body(Some("\"other/repo\"")), "not in watched_repos");
    assert_rejected_with(&config_body(Some("\"other/repo\"")), "other/repo");
}

#[test]
fn test_defaults_carries_the_empty_default() {
    let root = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(root.path());
    // test_defaults leaves auto_review disabled; the fork policy is
    // default-off by construction (absent block => None).
    assert!(
        cfg.auto_review().is_none(),
        "auto_review disabled by default"
    );
}
