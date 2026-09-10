//! Security posture catalogue (issue #324, DAR §15 security row).
//!
//! Pure mapping: every security-relevant test must exist as a named
//! `fn` in its suite file. Mirrors the
//! `tests/executor/certification_mapping_test.rs` discipline. This is
//! the CI-enforced catalogue that the security tests are wired — it
//! does NOT run them, it asserts their existence and registration.
//!
//! Coverage map (issue #324's four acceptance criteria):
//! - AC1 (corpus, no semantic change): the corpus harness below.
//! - AC2 (each denied sandbox op enforced): the live sandbox rows —
//!   host sentinel, daemon state, capabilities, network, and the RO
//!   `.git` shadow's git-metadata-mutation denial (#324's new test).
//! - AC3 (mutation violation + control-file tamper → Terminal +
//!   event): the review-integrity rows (rebadged, not duplicated).
//! - AC4 (fork PR never reaches the worker): the fork rows —
//!   predicate, discovery RowAction, and the #324 integration test.

const SECURITY_TESTS: &[(&str, &str, &str)] = &[
    // (area, suite file, fn name)
    // Corpus (hermetic, issue #324)
    (
        "corpus",
        "tests/security/corpus_test.rs",
        "corpus_loads_every_fixture_and_neutralises_each_vector",
    ),
    // Sandbox assertions (live-gated; existence is the contract)
    (
        "git-mutation-denial",
        "tests/executor/oci_isolation_live_test.rs",
        "git_metadata_mutation_denied_via_ro_shadow_live",
    ),
    (
        "git-shadow-write-rejection",
        "tests/executor/oci_isolation_live_test.rs",
        "git_shadow_write_rejected",
    ),
    (
        "host-sentinel",
        "tests/executor/oci_isolation_live_test.rs",
        "host_sentinel_unreachable_live",
    ),
    (
        "daemon-state",
        "tests/executor/oci_isolation_live_test.rs",
        "daemon_state_and_other_repos_unreachable_live",
    ),
    (
        "capabilities",
        "tests/executor/oci_isolation_live_test.rs",
        "capabilities_absent_no_new_privileges_live",
    ),
    (
        "network-closed",
        "tests/executor/oci_isolation_live_test.rs",
        "network_none_unreachable_live",
    ),
    // Mutation + control-file (hermetic; AC3 via #306's suite)
    (
        "mutation-violation",
        "tests/repo/review_integrity_test.rs",
        "tracked_file_modification_is_a_violation",
    ),
    (
        "control-file-tamper",
        "tests/repo/review_integrity_test.rs",
        "prompt_modification_detected_by_digest_not_dirty_check",
    ),
    (
        "mutation-event",
        "tests/repo/review_integrity_test.rs",
        "mutation_violation_event_is_emitted_with_dar13_shape",
    ),
    (
        "mutation-terminal",
        "tests/repo/review_integrity_test.rs",
        "finish_mutation_violation_routes_needs_attention_with_hint",
    ),
    // Fork adversarial (AC4)
    (
        "fork-gate-predicate",
        "tests/github/fork_gate_test.rs",
        "fork_row_classifies_fork_with_identity",
    ),
    (
        "fork-discovery-skip",
        "tests/daemon/review_discovery_test.rs",
        "fork_skips_with_head_repo_identity",
    ),
    (
        "fork-integration",
        "tests/security/fork_adversarial_test.rs",
        "fork_pr_fixture_never_enqueued",
    ),
    // Fork quarantine lifecycle (#337 Phase 2): the full allowed-fork
    // path — discovery → quarantine fetch → worktree → terminal →
    // quarantine removal.
    (
        "fork-quarantine-lifecycle",
        "tests/integration/fork_review_lifecycle_test.rs",
        "fork_review_lifecycle_end_to_end",
    ),
];

#[test]
fn every_security_test_is_defined_in_its_suite() {
    for (area, suite, fn_name) in SECURITY_TESTS {
        let path = std::path::Path::new(suite);
        let src = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("security catalogue: {area}: cannot read {suite}: {e}"));
        let needle = format!("fn {fn_name}(");
        assert!(
            src.contains(&needle),
            "security catalogue: {area} maps to {fn_name} which is not \
             defined as `fn {fn_name}(` in {suite}"
        );
    }
}

#[test]
fn every_security_test_binary_is_registered_in_cargo_toml() {
    let cargo = std::fs::read_to_string("Cargo.toml").expect("Cargo.toml");
    // Every suite file under tests/security/ needs a [[test]] entry.
    for entry in std::fs::read_dir("tests/security").expect("tests/security") {
        let p = entry.expect("dir entry").path();
        if p.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let stem = p.file_stem().unwrap().to_str().unwrap();
        let needle = format!("name = \"{stem}\"");
        assert!(
            cargo.contains(&needle),
            "security test binary {stem} has no [[test]] entry in Cargo.toml"
        );
    }
}
