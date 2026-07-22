//! Config failure stubs for the failure matrix (AC-15).
//!
//! Helpers that build invalid `RawConfig` shapes — missing
//! token, missing state_dir, unacknowledged reduced containment —
//! so `Config::from_raw` surfaces the expected typed error.

#![allow(dead_code)]

/// YAML that is syntactically valid but has no `caduceus:` mapping
/// and no `github_token` or `worker_command`. `Config::load_from`
/// will fail with a `Config` error.
pub const INVALID_CONFIG_NO_CADUCEUS: &str = "some_other_key: 42\n";

/// YAML where `reduced_containment_acknowledged` is explicitly
/// `false` and the `executor_mode` is `trusted_host`, so the
/// daemon must reject it with a config error (the
/// `ReducedContainmentNotAcknowledged` variant is on the
/// surface; the actual error returned is `Config(String)`).
pub const UNACKNOWLEDGED_CONTAINMENT_YAML: &str = r##"
caduceus:
  state_dir: "/tmp/test"
  api_base: "http://localhost:0"
  github_token: "ghp_test"
  worker_command:
    - "/bin/true"
  dry_run: true
  executor_mode: "trusted_host"
  reduced_containment_acknowledged: false
"##;

/// YAML with a valid `caduceus:` block but no
/// `github_token`, so token resolution must fail.
pub const MISSING_TOKEN_YAML: &str = r##"
caduceus:
  state_dir: "/tmp/test"
  api_base: "http://localhost:0"
  worker_command:
    - "/bin/true"
  dry_run: true
  reduced_containment_acknowledged: true
"##;
