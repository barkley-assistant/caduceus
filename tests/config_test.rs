//! Task 1.1 acceptance tests for `Config` parsing and validation.
//!
//! These tests exercise the [`Config`] parsing pipeline described in
//! the contract: `RawConfig` deserialisation, the public validator
//! (`Config::from_raw`), and the deterministic test defaults. The
//! env-aware resolution chain is the responsibility of Task 1.3 —
//! its tests will plug the same `Config::from_raw` body into the
//! end of the loader.

use std::path::Path;

use caduceus::config::{
    expand_leading_tilde, is_valid_repo_slug, Config, LoadContext, RawConfig, RawEnv,
    DEFAULT_API_BASE, DEFAULT_TICKET_LABEL_CODE, DEFAULT_TICKET_LABEL_INVESTIGATION,
};

fn ctx(root: &Path) -> LoadContext {
    LoadContext {
        hermes_home: Some(root.join("home")),
        plugin_root: Some(root.join("plugin")),
        env: RawEnv::default(),
    }
}

// ---------------------------------------------------------------------------
// Defaults & test_defaults
// ---------------------------------------------------------------------------

#[test]
fn test_defaults_match_contract() {
    let root = tempdir("defaults");
    let cfg = Config::test_defaults(&root);
    assert_eq!(cfg.poll_interval_seconds, 120);
    assert_eq!(cfg.worker_timeout_seconds, 3600);
    assert_eq!(cfg.http_timeout_seconds, 60);
    assert_eq!(cfg.git_timeout_seconds, 300);
    assert_eq!(cfg.transcript_max_bytes, 10 * 1024 * 1024);
    assert_eq!(cfg.run_retention_days, 30);
    assert_eq!(cfg.stale_run_hours, 1);
    assert_eq!(cfg.max_retries_per_issue, 3);
    assert_eq!(cfg.retry_backoff_seconds, 300);
    assert_eq!(cfg.ticket_label_code, DEFAULT_TICKET_LABEL_CODE);
    assert_eq!(
        cfg.ticket_label_investigation,
        DEFAULT_TICKET_LABEL_INVESTIGATION
    );
    assert_eq!(cfg.api_base, DEFAULT_API_BASE);
    assert_eq!(cfg.state_dir, root.join("state"));
    assert_eq!(cfg.log_path, root.join("state").join("processor.log"));
    assert_eq!(cfg.workdir_base, root.join("workdirs"));
    assert!(cfg.worker_command.contains(&"python3".to_string()));
    assert!(!cfg.dry_run);
    assert!(cfg.github_token.is_none());
    assert!(cfg.compiled_ignore_patterns.is_empty());
}

#[test]
fn raw_default_parses_minimal_plugin_derived_config() {
    // Minimal config: only the keys Hermes's plugin-defaults would set.
    let yaml = r#"
        worker_command: ["python3", "/path/to/bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("minimal yaml parses");
    let root = tempdir("minimal");
    let cfg = Config::from_raw(raw, &ctx(&root)).expect("minimal config validates");
    // Every default is filled in by ``Config::from_raw``.
    assert_eq!(cfg.poll_interval_seconds, 120);
    assert_eq!(cfg.ticket_label_code, DEFAULT_TICKET_LABEL_CODE);
    assert_eq!(
        cfg.ticket_label_investigation,
        DEFAULT_TICKET_LABEL_INVESTIGATION
    );
}

#[test]
fn every_default_is_independently_overridable() {
    let yaml = r#"
        poll_interval_seconds: 30
        worker_timeout_seconds: 10
        http_timeout_seconds: 5
        git_timeout_seconds: 15
        transcript_max_bytes: 1024
        run_retention_days: 7
        stale_run_hours: 2
        max_retries_per_issue: 1
        retry_backoff_seconds: 60
        ticket_label_code: "code-label"
        ticket_label_investigation: "investigate-label"
        api_base: "https://api.example.test"
        worker_command: ["python3", "/path/to/bridge.py"]
        dry_run: true
        watched_repos: ["acme/widgets"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let root = tempdir("override");
    let cfg = Config::from_raw(raw, &ctx(&root)).expect("config validates");
    assert_eq!(cfg.poll_interval_seconds, 30);
    assert_eq!(cfg.worker_timeout_seconds, 10);
    assert_eq!(cfg.http_timeout_seconds, 5);
    assert_eq!(cfg.git_timeout_seconds, 15);
    assert_eq!(cfg.transcript_max_bytes, 1024);
    assert_eq!(cfg.run_retention_days, 7);
    assert_eq!(cfg.stale_run_hours, 2);
    assert_eq!(cfg.max_retries_per_issue, 1);
    assert_eq!(cfg.retry_backoff_seconds, 60);
    assert_eq!(cfg.ticket_label_code, "code-label");
    assert_eq!(cfg.ticket_label_investigation, "investigate-label");
    assert_eq!(cfg.api_base, "https://api.example.test");
    assert!(cfg.dry_run);
    assert_eq!(cfg.watched_repos, vec!["acme/widgets".to_string()]);
}

// ---------------------------------------------------------------------------
// List replacement semantics
// ---------------------------------------------------------------------------

#[test]
fn explicit_lists_replace_defaults_not_append() {
    // The default comment_ignore_patterns is empty; the operator
    // wants a single explicit pattern. The result must not contain
    // any "default" patterns from elsewhere.
    let yaml = r#"
        comment_ignore_patterns: ["my-pattern"]
        comment_forbidden_strings: ["forbidden-A"]
        feedback_author_allowlist: ["alice"]
        worker_env_allowlist: ["OPENAI_*"]
        watched_repos: ["acme/widgets"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let root = tempdir("lists");
    let cfg = Config::from_raw(raw, &ctx(&root)).expect("config validates");
    assert_eq!(cfg.comment_ignore_patterns, vec!["my-pattern".to_string()]);
    assert_eq!(cfg.compiled_ignore_patterns.len(), 1);
    assert_eq!(
        cfg.comment_forbidden_strings,
        vec!["forbidden-A".to_string()]
    );
    assert_eq!(cfg.feedback_author_allowlist, vec!["alice".to_string()]);
    assert_eq!(cfg.worker_env_allowlist, vec!["OPENAI_*".to_string()]);
    assert_eq!(cfg.watched_repos, vec!["acme/widgets".to_string()]);
}

// ---------------------------------------------------------------------------
// Leading-tilde expansion
// ---------------------------------------------------------------------------

#[test]
fn leading_tilde_expands_to_home_directory() {
    let home = tempdir("tilde-home").join("realhome");
    std::fs::create_dir_all(&home).unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    let expanded = expand_leading_tilde(std::path::PathBuf::from("~/caduceus-state"));
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    assert!(expanded.starts_with(&home));
    assert!(expanded.ends_with("caduceus-state"));
}

#[test]
fn non_tilde_path_is_unchanged() {
    let path = std::path::PathBuf::from("/etc/caduceus");
    let expanded = expand_leading_tilde(path.clone());
    assert_eq!(expanded, path);
}

#[test]
fn embedded_tilde_is_not_expanded() {
    // Paths that contain a tilde not at the start must not be
    // shell-expanded — only a leading ``~`` triggers expansion
    // (CONTRACTS.md "Configuration").
    let path = std::path::PathBuf::from("/var/log/~caduceus");
    let expanded = expand_leading_tilde(path.clone());
    assert_eq!(expanded, path);
}

// ---------------------------------------------------------------------------
// ${plugin_root} expansion
// ---------------------------------------------------------------------------

#[test]
fn plugin_root_token_is_expanded_only_in_arguments() {
    let root = tempdir("plugin-root");
    let plugin_root = root.join("plugin");
    let mut ctx = ctx(&root);
    ctx.plugin_root = Some(plugin_root.clone());
    let yaml = r#"
        worker_command: ["python3", "${plugin_root}/plugin-assets/worker-bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let cfg = Config::from_raw(raw, &ctx).expect("config validates");
    assert_eq!(cfg.worker_command.len(), 2);
    assert_eq!(cfg.worker_command[0], "python3");
    assert!(cfg.worker_command[1].starts_with(&plugin_root.to_string_lossy().to_string()));
    assert!(cfg.worker_command[1].ends_with("worker-bridge.py"));
}

#[test]
fn plugin_root_token_in_program_position_is_rejected() {
    let root = tempdir("plugin-root-bad");
    let mut ctx = ctx(&root);
    ctx.plugin_root = Some(root.join("plugin"));
    let yaml = r#"
        worker_command: ["${plugin_root}/python3"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx).expect_err("must reject ${plugin_root} in argv[0]");
    let msg = format!("{err:?}");
    assert!(msg.contains("program position"), "got: {msg}");
}

#[test]
fn unknown_interpolation_is_rejected() {
    let root = tempdir("unknown-interp");
    let ctx = ctx(&root);
    let yaml = r#"
        worker_command: ["python3", "$HOME/something"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx).expect_err("must reject $HOME");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("unsupported interpolation") || msg.contains("forbidden interpolation"),
        "got: {msg}"
    );
}

#[test]
fn tilde_in_worker_command_is_rejected() {
    let root = tempdir("worker-tilde");
    let ctx = ctx(&root);
    let yaml = r#"
        worker_command: ["python3", "~/my-bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx).expect_err("must reject ~ in worker_command");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("forbidden interpolation") || msg.contains("unsupported interpolation"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Empty / zero rejection
// ---------------------------------------------------------------------------

#[test]
fn empty_worker_command_in_standalone_install_is_rejected() {
    let root = tempdir("empty-worker");
    let mut ctx = ctx(&root);
    // No plugin_root → default bridge path is unavailable → standalone install.
    ctx.plugin_root = None;
    let yaml = r#"
        worker_command: []
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx)
        .expect_err("empty worker_command must fail outside a plugin layout");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("worker_command is required for standalone"),
        "got: {msg}"
    );
}

#[test]
fn zero_durations_and_budgets_are_rejected() {
    let root = tempdir("zero-dur");
    let yaml = r#"
        poll_interval_seconds: 0
        worker_timeout_seconds: 0
        http_timeout_seconds: 0
        git_timeout_seconds: 0
        transcript_max_bytes: 0
        run_retention_days: 0
        stale_run_hours: 0
        max_retries_per_issue: 0
        retry_backoff_seconds: 0
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("zero values must fail");
    let msg = format!("{err:?}");
    // Spot-check that every relevant field is mentioned.
    assert!(msg.contains("poll_interval_seconds"), "got: {msg}");
    assert!(msg.contains("worker_timeout_seconds"), "got: {msg}");
    assert!(msg.contains("http_timeout_seconds"), "got: {msg}");
    assert!(msg.contains("git_timeout_seconds"), "got: {msg}");
    assert!(msg.contains("transcript_max_bytes"), "got: {msg}");
    assert!(msg.contains("run_retention_days"), "got: {msg}");
    assert!(msg.contains("stale_run_hours"), "got: {msg}");
    assert!(msg.contains("max_retries_per_issue"), "got: {msg}");
    assert!(msg.contains("retry_backoff_seconds"), "got: {msg}");
}

#[test]
fn empty_label_is_rejected() {
    let root = tempdir("empty-label");
    let yaml = r#"
        ticket_label_code: ""
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("empty label must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("ticket_label_code"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// Regex compilation
// ---------------------------------------------------------------------------

#[test]
fn invalid_regex_is_rejected_at_config_time() {
    let root = tempdir("invalid-regex");
    let yaml = r#"
        comment_ignore_patterns: ["("]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("invalid regex must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("invalid regex"), "got: {msg}");
}

#[test]
fn default_case_sensitive_matching_is_not_flipped_by_inner_flags() {
    // A pattern with NO ``(?i)`` flag matches case-sensitively.
    let root = tempdir("case-sensitive");
    let yaml = r#"
        comment_ignore_patterns: ["Dependabot"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let cfg = Config::from_raw(raw, &ctx(&root)).expect("config validates");
    assert_eq!(cfg.compiled_ignore_patterns.len(), 1);
    let re = &cfg.compiled_ignore_patterns[0];
    // Case-sensitive: only the exact casing matches.
    assert!(!re.is_match("dependabot[bot]"));
    assert!(!re.is_match("DEPENDABOT[bot]"));
    assert!(re.is_match("Dependabot[bot]"));
}

#[test]
fn explicit_ci_flag_in_pattern_enables_case_insensitive_match() {
    let root = tempdir("case-insensitive");
    let yaml = r#"
        comment_ignore_patterns: ["(?i)Dependabot"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let cfg = Config::from_raw(raw, &ctx(&root)).expect("config validates");
    let re = &cfg.compiled_ignore_patterns[0];
    assert!(re.is_match("dependabot[bot]"));
    assert!(re.is_match("DEPENDABOT[bot]"));
    assert!(re.is_match("Dependabot[bot]"));
}

// ---------------------------------------------------------------------------
// Repository slug validation
// ---------------------------------------------------------------------------

#[test]
fn valid_repo_slugs_pass() {
    for slug in [
        "acme/widgets",
        "owner-with-hyphens/repo.with-dots",
        "a/b",
        "Owner/Repo",
    ] {
        assert!(is_valid_repo_slug(slug), "expected valid: {slug}");
    }
}

#[test]
fn invalid_repo_slugs_fail() {
    for slug in [
        "",
        "no-slash",
        "owner/",
        "/repo",
        "owner/..",
        "-leading-hyphen/repo",
        "trailing-hyphen-/repo",
        "owner/this-has-a/slash",
    ] {
        assert!(!is_valid_repo_slug(slug), "expected invalid: {slug:?}");
    }
}

#[test]
fn duplicate_watched_repos_are_rejected_case_insensitively() {
    let root = tempdir("dup-repos");
    let yaml = r#"
        watched_repos: ["Acme/widgets", "acme/Widgets"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("duplicate repos must fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("duplicate") || msg.contains("Duplicate"),
        "got: {msg}"
    );
}

#[test]
fn invalid_watched_repo_slug_is_rejected() {
    let root = tempdir("bad-repo");
    let yaml = r#"
        watched_repos: ["owner with space/repo"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("invalid repo must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("not owner/repo"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// Duplicate / equal labels
// ---------------------------------------------------------------------------

#[test]
fn duplicate_trigger_labels_are_rejected() {
    let root = tempdir("dup-labels");
    let yaml = r#"
        ticket_label_code: "auto-fix"
        ticket_label_investigation: "auto-fix"
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("duplicate labels must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("must differ"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// Deny unknown fields
// ---------------------------------------------------------------------------

#[test]
fn unknown_yaml_field_is_rejected() {
    let yaml = r#"
        poll_interval_seconds: 60
        worker_command: ["python3", "bridge.py"]
        not_a_real_field: 123
        "#;
    let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
    assert!(
        result.is_err(),
        "deny_unknown_fields must reject unknown keys"
    );
}

// ---------------------------------------------------------------------------
// Worker env allowlist validation
// ---------------------------------------------------------------------------

#[test]
fn allowlist_rejects_github_credential_exact_match() {
    let root = tempdir("allowlist-creds");
    let yaml = r#"
        worker_env_allowlist: ["PATH", "GITHUB_TOKEN"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("GITHUB_TOKEN must be denied");
    let msg = format!("{err:?}");
    assert!(msg.contains("denied credential"), "got: {msg}");
}

#[test]
fn allowlist_rejects_github_credential_prefix_wildcard() {
    let root = tempdir("allowlist-creds-prefix");
    let yaml = r#"
        worker_env_allowlist: ["PATH", "GITHUB_*"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("GITHUB_* must be denied");
    let msg = format!("{err:?}");
    assert!(msg.contains("denied credential"), "got: {msg}");
}

#[test]
fn allowlist_rejects_malformed_entry() {
    let root = tempdir("allowlist-malformed");
    let yaml = r#"
        worker_env_allowlist: ["BAD=NAME", "", "has spaces"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("malformed allowlist must fail");
    let msg = format!("{err:?}");
    // Multiple malformed entries each contribute an error.
    assert!(msg.contains("must not contain '='"), "got: {msg}");
    assert!(msg.contains("must not be empty"), "got: {msg}");
    assert!(msg.contains("non-portable characters"), "got: {msg}");
}

#[test]
fn allowlist_rejects_internal_wildcard() {
    let root = tempdir("allowlist-internal-wildcard");
    let yaml = r#"
        worker_env_allowlist: ["OP*EN"]
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("internal wildcard must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("terminal '*' wildcard"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// Secure-path semantics
// ---------------------------------------------------------------------------

#[test]
fn state_dir_must_not_be_a_symlink() {
    let root = tempdir("symlinked-state");
    let target = root.join("real-state");
    std::fs::create_dir_all(&target).unwrap();
    let symlink = root.join("linked-state");
    std::os::unix::fs::symlink(&target, &symlink).unwrap();
    let yaml = format!(
        r#"
        state_dir: "{}"
        worker_command: ["python3", "bridge.py"]
        "#,
        symlink.display()
    );
    let raw: RawConfig = serde_yaml::from_str(&yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("symlinked state dir must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("must not be a symlink"), "got: {msg}");
}

#[test]
fn state_dir_must_not_be_a_file() {
    let root = tempdir("state-is-file");
    let file = root.join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let yaml = format!(
        r#"
        state_dir: "{}"
        worker_command: ["python3", "bridge.py"]
        "#,
        file.display()
    );
    let raw: RawConfig = serde_yaml::from_str(&yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("file-as-state-dir must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("not a directory"), "got: {msg}");
}

#[test]
fn state_dir_must_not_be_empty() {
    let root = tempdir("empty-state");
    let yaml = r#"
        state_dir: ""
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx(&root)).expect_err("empty state dir must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("must not be empty"), "got: {msg}");
}

#[test]
fn existing_state_dir_is_accepted_when_directory() {
    let root = tempdir("ok-state");
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let yaml = format!(
        r#"
        state_dir: "{}"
        worker_command: ["python3", "bridge.py"]
        "#,
        state.display()
    );
    let raw: RawConfig = serde_yaml::from_str(&yaml).expect("yaml parses");
    Config::from_raw(raw, &ctx(&root)).expect("real directory must be accepted");
}

#[test]
fn missing_state_dir_path_is_accepted() {
    let root = tempdir("missing-state");
    let yaml = format!(
        r#"
        state_dir: "{}/state-not-yet"
        worker_command: ["python3", "bridge.py"]
        "#,
        root.display()
    );
    let raw: RawConfig = serde_yaml::from_str(&yaml).expect("yaml parses");
    Config::from_raw(raw, &ctx(&root))
        .expect("missing state dir is fine — daemon creates it later");
}

// ---------------------------------------------------------------------------
// Standalone missing-worker error
// ---------------------------------------------------------------------------

#[test]
fn missing_worker_command_in_standalone_install_is_a_config_error() {
    let root = tempdir("standalone-no-worker");
    let mut ctx = ctx(&root);
    ctx.plugin_root = None; // standalone install
    let yaml = r#"
        poll_interval_seconds: 60
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = Config::from_raw(raw, &ctx)
        .expect_err("standalone install without worker_command must fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("worker_command is required for standalone"),
        "got: {msg}"
    );
}

#[test]
fn hermes_primary_install_resolves_default_bridge() {
    // With a plugin_root present, the default worker command is the
    // canonical bridge under plugin-assets/.
    let root = tempdir("hermes-primary");
    let mut ctx = ctx(&root);
    ctx.plugin_root = Some(root.join("plugin"));
    let yaml = r#"
        poll_interval_seconds: 60
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let cfg = Config::from_raw(raw, &ctx).expect("default bridge path resolves");
    assert_eq!(
        cfg.worker_command.first().map(String::as_str),
        Some("python3")
    );
    let bridge_arg = cfg.worker_command.get(1).expect("bridge path argument");
    assert!(
        bridge_arg.ends_with("worker-bridge.py"),
        "got: {bridge_arg}"
    );
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn tempdir(label: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-config-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}
