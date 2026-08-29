//! Config-time `sandbox.pass_env` deny validation (issue #249; spec
//! "Config-time pass_env deny validation", design D2).
//!
//! Every rejection is PRESENCE-INDEPENDENT: a denied credential name,
//! a reserved canonical/compat key, or a malformed name fails config
//! load whether or not the variable exists in the daemon process
//! environment. Allowed names load fine (resolution behavior is a
//! separate concern, spec R3).
//!
//! Error channel: `Config::from_raw` aggregation →
//! `CaduceusError::Config` carrying the per-entry error lines.

use caduceus::infra::config::Config;
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

fn with_pass_env(entries: &str) -> String {
    format!(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"__TMP__/state\"\n\
         reduced_containment_acknowledged: true\n\
         sandbox:\n\
         \x20 image: \"{VALID_IMAGE}\"\n\
         \x20 pass_env: [{entries}]\n"
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

/// Restore helper for the presence test.
struct VarGuard(&'static str);
impl Drop for VarGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

// ---------------------------------------------------------------------------
// Presence-independent rejection of denied credential names (spec R4)
// ---------------------------------------------------------------------------

/// Denied name with the variable PRESENT in the daemon env — refused.
#[test]
fn denied_name_rejected_with_env_present() {
    // Set the variable so the "present" half of the presence matrix
    // is genuinely exercised.
    let var = "CADUCEUS_PASS_ENV_TEST_PRESENT_TOKEN";
    std::env::set_var(var, "some-credential-value");
    let _guard = VarGuard(var);

    for name in ["GITHUB_TOKEN", "GH_TOKEN", "CADUCEUS_GITHUB_TOKEN"] {
        assert_rejected_with(&with_pass_env(&format!("\"{name}\"")), name);
    }
}

/// Denied name with the variable ABSENT from the daemon env —
/// still refused.
#[test]
fn denied_name_rejected_with_env_absent() {
    // Ensure the variable really is absent (presence-independent).
    std::env::remove_var("CADUCEUS_PASS_ENV_TEST_ABSENT_TOKEN");
    assert_rejected_with(&with_pass_env("\"GITHUB_TOKEN\""), "GITHUB_TOKEN");
    assert_rejected_with(&with_pass_env("\"GH_TOKEN\""), "GH_TOKEN");
    assert_rejected_with(
        &with_pass_env("\"AUTO_ISSUE_GITHUB_TOKEN\""),
        "AUTO_ISSUE_GITHUB_TOKEN",
    );
}

/// Substring rule: any `GITHUB`∧`TOKEN` name is refused.
#[test]
fn github_token_substring_names_rejected() {
    assert_rejected_with(&with_pass_env("\"MY_GITHUB_TOKEN\""), "MY_GITHUB_TOKEN");
}

/// Daemon-internal rule: `CADUCEUS_*`∧(`SECRET`|`TOKEN`) refused.
#[test]
fn caduceus_internal_marker_names_rejected() {
    assert_rejected_with(
        &with_pass_env("\"CADUCEUS_MY_SECRET\""),
        "CADUCEUS_MY_SECRET",
    );
    assert_rejected_with(&with_pass_env("\"CADUCEUS_MY_TOKEN\""), "CADUCEUS_MY_TOKEN");
}

// ---------------------------------------------------------------------------
// Reserved-key collisions (design D2 item 3)
// ---------------------------------------------------------------------------

#[test]
fn canonical_reserved_keys_rejected() {
    for name in [
        "CADUCEUS_RUN_ID",
        "CADUCEUS_RESULT_PATH",
        "CADUCEUS_WORKTREE_PATH",
    ] {
        assert_rejected_with(&with_pass_env(&format!("\"{name}\"")), name);
    }
}

#[test]
fn compat_reserved_keys_rejected() {
    assert_rejected_with(&with_pass_env("\"HOME\""), "HOME");
    assert_rejected_with(&with_pass_env("\"TMPDIR\""), "TMPDIR");
}

// ---------------------------------------------------------------------------
// Name charset (design D2 item 4)
// ---------------------------------------------------------------------------

#[test]
fn malformed_charset_rejected() {
    // Leading digit.
    assert_rejected_with(&with_pass_env("\"1VAR\""), "1VAR");
    // Injection shapes: `=` and a comment marker. (A raw newline in a
    // double-quoted YAML scalar is folded to a space by the YAML
    // parser before validation, so it still fails the charset check
    // — covered by the "VAR X" rejection below.)
    assert_rejected_with(&with_pass_env("\"VAR=X\""), "VAR=X");
    assert_rejected_with(&with_pass_env("\"#comment\""), "#comment");
    assert_rejected_with(&with_pass_env("\"VAR\nX\""), "must match");
}

// ---------------------------------------------------------------------------
// Allowed names load (spec scenario)
// ---------------------------------------------------------------------------

#[test]
fn allowed_names_load() {
    let cfg = load(&with_pass_env("\"MY_TOOL_TOKEN\", \"CI_NODE_INDEX\""))
        .expect("allowed names must load");
    let sb = cfg.sandbox();
    assert_eq!(
        sb.pass_env,
        vec!["MY_TOOL_TOKEN".to_string(), "CI_NODE_INDEX".to_string()]
    );
}

#[test]
fn empty_pass_env_loads() {
    let cfg = load(&with_pass_env("")).expect("empty pass_env must load");
    assert!(cfg.sandbox().pass_env.is_empty());
}
