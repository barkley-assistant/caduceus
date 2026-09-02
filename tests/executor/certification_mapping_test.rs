//! Certification suite self-check (issue #252): every issue checklist
//! item must map to a named test. The mapping below is the single
//! source of truth for the checklist→test table; it is asserted here
//! (each mapped function name must actually exist in the suite source)
//! and rendered in `docs/certification/oci-certification.md`.
//!
//! This is a PURE test — no Docker engine is needed — so it runs on
//! every CI leg and keeps the mapping honest without runtime test
//! introspection.
//!
//! Convention for `fn_name` entries: the exact `fn` name as it
//! appears in the source file. "EXISTING" = already present before
//! issue #252; "NEW" = added by the live certification suite.

/// (checklist item, test function name).
const MAPPING: &[(&str, &str)] = &[
    // 1. cannot read host sentinel outside allowed mounts
    (
        "cannot read host sentinel outside allowed mounts",
        "host_sentinel_unreachable_live",
    ),
    // 2. cannot access daemon state; cannot access other repositories
    (
        "cannot access daemon state; cannot access other repositories",
        "daemon_state_and_other_repos_unreachable_live",
    ),
    // 3. `.git` does not reveal daemon Git metadata (pointer + dir shadows)
    (
        ".git does not reveal daemon Git metadata",
        "git_shadow_read_sees_only_shadow",
    ),
    // 4. workspace writable; rootfs read-only; output/result path works
    (
        "workspace writable; rootfs read-only; output path works",
        "workspace_writable_rootfs_readonly_output_writes_live",
    ),
    // 5. writable mount surfaces == {/workspace, /output} + bounded {/tmp, /dev/shm}
    (
        "writable mount surfaces enumerated",
        "oci_mount_enumeration_two_writable_surfaces",
    ),
    // 6. capabilities absent (cap-drop ALL from inside); no-new-privileges holds
    (
        "capabilities absent; no-new-privileges holds",
        "capabilities_absent_no_new_privileges_live",
    ),
    // 7. runtime socket absent; device access unavailable
    (
        "runtime socket absent; device access unavailable",
        "runtime_socket_and_device_absent_live",
    ),
    // 8. memory hog constrained; fork bomb PID-constrained; CPU burn constrained
    ("memory hog constrained", "memory_hog_oom_live"),
    ("fork bomb PID-constrained", "fork_bomb_eagain_live"),
    ("CPU burn constrained", "cpu_burn_throttled_live"),
    // 9. /tmp bounded; /dev/shm bounded
    ("/tmp bounded", "tmpfs_bounded_live"),
    ("/dev/shm bounded", "dev_shm_bounded_live"),
    // 10. network:none unreachable; unrestricted works AND is not host networking
    (
        "network none cannot reach network",
        "network_none_unreachable_live",
    ),
    (
        "unrestricted works and is not host networking",
        "unrestricted_not_host_live",
    ),
    // 11. daemon GitHub credentials absent; explicit pass_env present;
    //     unapproved environment absent; missing pass_env aborts pre-create
    (
        "daemon GitHub credentials absent",
        "oci_container_env_is_exact_canonical_plus_pass_env",
    ),
    (
        "explicit pass_env variable present",
        "oci_container_env_is_exact_canonical_plus_pass_env",
    ),
    (
        "unapproved environment absent",
        "oci_container_env_is_exact_canonical_plus_pass_env",
    ),
    (
        "missing requested pass_env name aborts pre-create",
        "missing_pass_env_aborts_pre_create",
    ),
    // 12. timeout cleans container; cancellation cleans container
    ("timeout cleans container", "timeout_cleans_container_live"),
    (
        "cancellation cleans container",
        "cancellation_cleans_container_live",
    ),
    // 13. simulated daemon crash/restart reconciles the orphan;
    //     heartbeat advances during a live run
    (
        "simulated daemon crash/restart reconciles orphan",
        "crash_restart_orphan_reconcile_live",
    ),
    (
        "heartbeat advances during live run",
        "heartbeat_advances_during_run_live",
    ),
    // 14. wrong-digest image rejected before execution
    (
        "wrong-digest image rejected before execution",
        "wrong_digest_rejected_before_execution_live",
    ),
    // 15. rootful identity correct; rootless identity correct
    ("rootful identity correct", "rootful_docker_identity_canary"),
    (
        "rootless identity correct",
        "rootless_docker_identity_canary",
    ),
    // 16. custom unrelated worker image succeeds (image neutrality)
    (
        "custom unrelated worker image succeeds",
        "image_neutrality_custom_unrelated_image_live",
    ),
];

/// Every checklist item maps to a named test that actually exists in
/// the suite sources (a `fn <name>(` definition, not a comment).
#[test]
fn every_checklist_item_maps_to_a_named_test() {
    // include_str! needs literal paths; SUITE_FILES is documentation.
    let sources = [
        include_str!("oci_isolation_live_test.rs").replace("\r\n", "\n"),
        include_str!("../integration/oci_env_live_test.rs").replace("\r\n", "\n"),
        include_str!("credential_leak_test.rs").replace("\r\n", "\n"),
        include_str!("oci_image_verify_test.rs").replace("\r\n", "\n"),
        include_str!("oci_lifecycle_stub_test.rs").replace("\r\n", "\n"),
    ];

    for (item, fn_name) in MAPPING {
        let defined = sources
            .iter()
            .any(|src| src.contains(&format!("fn {fn_name}(")));
        assert!(
            defined,
            "checklist item {item:?} maps to {fn_name} which is not defined \
             as `fn {fn_name}(` in any suite file"
        );
    }
}

/// Every mapped name uses the `_live` suffix for engine-driven cases
/// (so the mapping table's "live vs pure" column stays honest).
#[test]
fn mapping_names_are_consistent() {
    let pure_ok = [
        "oci_container_env_is_exact_canonical_plus_pass_env",
        "missing_pass_env_aborts_pre_create",
        "git_shadow_read_sees_only_shadow",
        "oci_mount_enumeration_two_writable_surfaces",
        "rootful_docker_identity_canary",
        "rootless_docker_identity_canary",
    ];
    for (item, fn_name) in MAPPING {
        if pure_ok.contains(fn_name) {
            continue;
        }
        assert!(
            fn_name.ends_with("_live"),
            "engine-driven checklist item {item:?} must map to a *_live test, got {fn_name}"
        );
    }
}
