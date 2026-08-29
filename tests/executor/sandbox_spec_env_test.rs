//! Resolution-matrix tests for the frozen OCI `pass_env` semantics
//! (issue #249; spec R1/R3/R4/R7; design D3/D8).
//!
//! The resolver MUST consult only the injected parent-environment
//! map: present ⇒ included; absent ⇒ typed error naming the variable
//! (never warn-and-skip); denied names refused presence-independently
//! at resolution; and the assembled env is exactly canonical 11 +
//! compat + resolved `pass_env` — nothing else, regardless of any
//! TrustedHost allowlist configuration.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;

use caduceus::executor::sandbox_spec::{resolve_with_env, CANONICAL_ENV_KEYS, COMPAT_ENV_KEYS};
use caduceus::infra::config::Config;
use caduceus::infra::error::{CaduceusError, CaduceusResult};

mod support;

fn base_cfg() -> (Config, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(tmp.path());
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    (cfg, worktree)
}

fn resolve_with(
    cfg: &Config,
    worktree: &std::path::Path,
    pass_env: &[&str],
    parent_env: &BTreeMap<String, String>,
) -> CaduceusResult<caduceus::executor::SandboxSpec> {
    let mut cfg = cfg.clone();
    cfg.sandbox.as_mut().expect("sandbox").pass_env =
        pass_env.iter().map(|s| s.to_string()).collect();
    let runtime = support::runtime_facts(&cfg, "run-001", worktree);
    let os_env: BTreeMap<OsString, OsString> = parent_env
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &support::executor_spec(&runtime),
        &os_env,
    )
}

fn env_map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Resolution matrix (spec R3)
// ---------------------------------------------------------------------------

/// Present name ⇒ its exact daemon value is included in the
/// assembled environment.
#[test]
fn present_name_is_included() {
    let (cfg, worktree) = base_cfg();
    let parent = env_map(&[("MY_TOOL_TOKEN", "abc123")]);
    let spec = resolve_with(&cfg, &worktree, &["MY_TOOL_TOKEN"], &parent).expect("must resolve");
    let env = spec.environment();
    assert!(
        env.contains(&("MY_TOOL_TOKEN".to_string(), "abc123".to_string())),
        "resolved value must be present, got: {env:?}"
    );
}

/// Absent name ⇒ typed error that names the variable, never the
/// value; nothing is silently skipped.
#[test]
fn absent_name_fails_with_typed_error_naming_the_var() {
    let (cfg, worktree) = base_cfg();
    let parent = env_map(&[("OTHER_VAR", "secret-other-value")]);
    let err = resolve_with(&cfg, &worktree, &["MISSING_VAR"], &parent)
        .expect_err("absent name must fail");
    match &err {
        CaduceusError::Config(msg) => {
            assert!(
                msg.contains("MISSING_VAR"),
                "error must name the variable: {msg}"
            );
            assert!(
                !msg.contains("secret-other-value"),
                "error must never contain a value: {msg}"
            );
        }
        other => panic!("expected CaduceusError::Config; got: {other:?}"),
    }
}

/// Denied name PRESENT in the parent env ⇒ refused at resolution
/// (defensive re-check; spec R4).
#[test]
fn denied_name_present_is_refused() {
    let (cfg, worktree) = base_cfg();
    let parent = env_map(&[("GITHUB_TOKEN", "ghs_secret")]);
    let err = resolve_with(&cfg, &worktree, &["GITHUB_TOKEN"], &parent)
        .expect_err("denied present must be refused");
    match &err {
        CaduceusError::Config(msg) => {
            assert!(
                msg.contains("GITHUB_TOKEN") && msg.contains("denied"),
                "refusal must identify the denied name: {msg}"
            );
            assert!(
                !msg.contains("ghs_secret"),
                "refusal must never contain the value: {msg}"
            );
        }
        other => panic!("expected CaduceusError::Config; got: {other:?}"),
    }
}

/// Denied name ABSENT from the parent env ⇒ still refused
/// (presence-independent; spec R4).
#[test]
fn denied_name_absent_is_refused() {
    let (cfg, worktree) = base_cfg();
    let parent = BTreeMap::new();
    let err = resolve_with(&cfg, &worktree, &["GH_TOKEN"], &parent)
        .expect_err("denied absent must be refused");
    assert!(
        matches!(err, CaduceusError::Config(ref m) if m.contains("GH_TOKEN")),
        "expected refusal naming GH_TOKEN; got: {err:?}"
    );
}

/// CADUCEUS-prefixed secret/token names are refused by the shared
/// deny union regardless of presence.
#[test]
fn caduceus_prefixed_secret_names_are_refused() {
    let (cfg, worktree) = base_cfg();
    for name in ["CADUCEUS_MY_SECRET", "CADUCEUS_MY_TOKEN"] {
        let parent = env_map(&[(name, "x")]);
        let err = resolve_with(&cfg, &worktree, &[name], &parent)
            .expect_err("CADUCEUS secret name must be refused");
        assert!(
            matches!(err, CaduceusError::Config(ref m) if m.contains(name)),
            "expected refusal naming {name}; got: {err:?}"
        );
    }
}

/// A non-UTF-8 daemon value is refused with a typed error naming the
/// variable — never warn-and-skip, never the value.
#[test]
fn non_utf8_value_is_refused_naming_the_var() {
    let (cfg, worktree) = base_cfg();
    let mut os_env: BTreeMap<OsString, OsString> = BTreeMap::new();
    os_env.insert(
        OsString::from("BAD_BYTES"),
        OsString::from_vec(vec![0xff, 0xfe]),
    );
    let mut cfg = cfg.clone();
    cfg.sandbox.as_mut().expect("sandbox").pass_env = vec!["BAD_BYTES".to_string()];
    let runtime = support::runtime_facts(&cfg, "run-001", &worktree);
    let err = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &support::executor_spec(&runtime),
        &os_env,
    )
    .expect_err("non-UTF-8 value must be refused");
    match &err {
        CaduceusError::Config(msg) => {
            assert!(msg.contains("BAD_BYTES"), "must name the var: {msg}");
        }
        other => panic!("expected CaduceusError::Config; got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Full assembled env snapshots (spec R1)
// ---------------------------------------------------------------------------

/// Empty `pass_env` ⇒ EXACTLY the canonical 11 + `HOME`/`TMPDIR`,
/// sorted by key — provably nothing else.
#[test]
fn empty_pass_env_snapshot_is_canonical_plus_compat_sorted() {
    let (cfg, worktree) = base_cfg();
    let parent = env_map(&[("SOME_RANDOM_HOST_VAR", "host-noise")]);
    let spec = resolve_with(&cfg, &worktree, &[], &parent).expect("must resolve");
    let mut expected: Vec<(String, String)> = CANONICAL_ENV_KEYS
        .iter()
        .map(|k| (k.to_string(), String::new()))
        .collect();
    for k in COMPAT_ENV_KEYS {
        expected.push((k.to_string(), "/tmp".to_string()));
    }
    expected.sort();
    let env = spec.environment();
    let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys.len(), 13, "exactly 13 entries, got: {keys:?}");
    for (entry, (key, _expected)) in env.iter().zip(expected.iter()) {
        assert_eq!(&entry.0, key, "sorted key order");
        if *key == "HOME" || *key == "TMPDIR" {
            assert_eq!(entry.1, "/tmp", "compat value");
        }
    }
    // Provably no host inheritance.
    assert!(
        !keys.contains(&"SOME_RANDOM_HOST_VAR"),
        "unapproved host variable must be absent: {keys:?}"
    );
    // Canonical container-side paths.
    let find = |k: &str| env.iter().find(|e| e.0 == k).map(|e| e.1.clone());
    assert_eq!(
        find("CADUCEUS_WORKTREE_PATH").as_deref(),
        Some("/workspace")
    );
    assert_eq!(
        find("CADUCEUS_RESULT_PATH").as_deref(),
        Some("/output/worker-result.json")
    );
}

/// Non-empty `pass_env` ⇒ superset of canonical + compat with the
/// resolved values, sorted keys.
#[test]
fn non_empty_pass_env_snapshot_is_sorted_superset() {
    let (cfg, worktree) = base_cfg();
    let parent = env_map(&[("ALPHA", "a-value"), ("BETA", "b-value")]);
    let spec = resolve_with(&cfg, &worktree, &["BETA", "ALPHA"], &parent).expect("must resolve");
    let env = spec.environment();
    let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "keys must be sorted");
    assert_eq!(keys.len(), 15, "11 canonical + 2 compat + 2 pass_env");
    let find = |k: &str| env.iter().find(|e| e.0 == k).map(|e| e.1.clone());
    assert_eq!(find("ALPHA").as_deref(), Some("a-value"));
    assert_eq!(find("BETA").as_deref(), Some("b-value"));
    for k in CANONICAL_ENV_KEYS {
        assert!(keys.contains(k), "canonical {k} must remain");
    }
    assert!(keys.contains(&"HOME") && keys.contains(&"TMPDIR"));
}

// ---------------------------------------------------------------------------
// `resolve` delegation smoke test
// ---------------------------------------------------------------------------

/// The plain `resolve` delegates to `resolve_with_env` with the
/// captured `std::env` snapshot: a name set in the process env is
/// resolved exactly like an injected map entry.
#[test]
fn resolve_delegates_to_std_env_snapshot() {
    struct EnvGuard(&'static str);
    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            EnvGuard(name)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }
    let _guard = EnvGuard::set("CADUCEUS_DELEGATION_SMOKE_VAR", "delegated");

    let (mut cfg, worktree) = base_cfg();
    cfg.sandbox.as_mut().expect("sandbox").pass_env =
        vec!["CADUCEUS_DELEGATION_SMOKE_VAR".to_string()];
    let runtime = support::runtime_facts(&cfg, "run-001", &worktree);
    let spec = caduceus::executor::sandbox_spec::resolve(
        cfg.sandbox(),
        &runtime,
        &support::executor_spec(&runtime),
    )
    .expect("must resolve");
    let env = spec.environment();
    assert!(
        env.contains(&(
            "CADUCEUS_DELEGATION_SMOKE_VAR".to_string(),
            "delegated".to_string()
        )),
        "std::env snapshot entry must resolve, got: {env:?}"
    );
}

// ---------------------------------------------------------------------------
// TrustedHost pinning (spec R7)
// ---------------------------------------------------------------------------

/// A config with `worker_env_allowlist` set does NOT change the
/// assembled OCI env: OCI ignores `worker_env_allowlist`,
/// `sanitized_env`, and `DEFAULT_ALLOWLIST_*` entirely — no
/// host-env inheritance semantics are imported.
#[test]
fn worker_env_allowlist_does_not_change_oci_env() {
    let (mut cfg, worktree) = base_cfg();
    cfg.worker_env_allowlist = vec![
        "SOME_RANDOM_HOST_VAR".to_string(),
        "PATH".to_string(),
        "HOME".to_string(),
    ];
    let runtime = support::runtime_facts(&cfg, "run-001", &worktree);
    let spec = caduceus::executor::sandbox_spec::resolve(
        cfg.sandbox(),
        &runtime,
        &support::executor_spec(&runtime),
    )
    .expect("must resolve");
    let keys: Vec<&str> = spec.environment().iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys.len(), 13, "canonical + compat only, got: {keys:?}");
    for k in CANONICAL_ENV_KEYS {
        assert!(keys.contains(k));
    }
    assert!(keys.contains(&"HOME") && keys.contains(&"TMPDIR"));
}
