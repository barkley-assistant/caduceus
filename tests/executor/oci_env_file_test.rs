//! Env-file transport tests for the frozen OCI worker-env contract
//! (issue #249; spec "Env-file security and lifecycle", design D5).
//!
//! Pinned invariants:
//!
//! - the file is created mode 0600 with a unique randomly minted
//!   `caduceus_env_<random>.env` name inside the daemon-private run
//!   dir (never shared `std::env::temp_dir()`);
//! - contents are exactly the sorted `KEY=VALUE\n` lines;
//! - `Drop` deletes the file on every exit path (including unwind),
//!   idempotently;
//! - a name collision on the unique-name mint is retried (bounded);
//! - a lifecycle create-failure driven by a fake engine leaves no
//!   env file behind (the deletion guard is dropped before the
//!   create result is propagated).

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serial_test::serial;
use tokio_util::sync::CancellationToken;

use caduceus::executor::oci_env_file::OciEnvFile;
use caduceus::executor::oci_lifecycle;
use caduceus::executor::sandbox_spec::{resolve, SandboxEngine};
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::{CaduceusError, CaduceusResult};
use caduceus::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};

mod support;

fn env_map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Location: daemon-private run dir, never shared temp_dir (design D5)
// ---------------------------------------------------------------------------

#[test]
fn file_lives_in_the_run_dir_not_temp_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let run_dir = tmp.path().join("state").join("oci-runs").join("run-loc");
    let file = OciEnvFile::create(&run_dir, &env_map(&[("A", "1")])).expect("create");
    let path = file.path();
    assert!(
        path.starts_with(&run_dir),
        "env file must live in the run dir, got: {}",
        path.display()
    );
    // The placeholder's flaw was dropping files DIRECTLY into shared
    // temp_dir; the env file must always be nested inside the
    // daemon-private run dir instead.
    assert_ne!(
        path.parent(),
        Some(std::env::temp_dir().as_path()),
        "env file must never be a direct child of shared temp_dir: {}",
        path.display()
    );
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    assert!(
        file_name.starts_with("caduceus_env_") && file_name.ends_with(".env"),
        "file name must match caduceus_env_<random>.env, got: {file_name}"
    );
    // The run dir was created mode 0700 (design D5).
    let dir_meta = std::fs::metadata(&run_dir).expect("run dir metadata");
    assert_eq!(
        dir_meta.permissions().mode() & 0o777,
        0o700,
        "run dir must be private (0700)"
    );
}

// ---------------------------------------------------------------------------
// Creation: mode 0600, exclusive handle (spec "File permissions are 0600")
// ---------------------------------------------------------------------------

#[test]
fn file_mode_is_0600() {
    let tmp = tempfile::tempdir().expect("tmp");
    let file = OciEnvFile::create(
        &tmp.path().join("oci-runs").join("run-mode"),
        &env_map(&[("A", "1")]),
    )
    .expect("create");
    let meta = std::fs::metadata(file.path()).expect("metadata");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "env file mode must be 0600, got {:o}",
        meta.permissions().mode() & 0o777
    );
}

// ---------------------------------------------------------------------------
// Random unique name (spec "File name is random and non-deterministic")
// ---------------------------------------------------------------------------

#[test]
fn two_creates_mint_different_names() {
    let tmp = tempfile::tempdir().expect("tmp");
    let run_dir = tmp.path().join("oci-runs").join("run-random");
    let a = OciEnvFile::create(&run_dir, &env_map(&[("A", "1")])).expect("create a");
    let b = OciEnvFile::create(&run_dir, &env_map(&[("A", "1")])).expect("create b");
    let name = |f: &OciEnvFile| {
        f.path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string()
    };
    assert_ne!(name(&a), name(&b), "consecutive creates mint unique names");
    // Both exist until dropped (the guard owns the lifecycle).
    assert!(a.path().exists() && b.path().exists());
    drop(a);
    drop(b);
}

// ---------------------------------------------------------------------------
// Collision retry (bounded)
// ---------------------------------------------------------------------------

#[test]
fn collision_on_unique_name_is_retried() {
    let tmp = tempfile::tempdir().expect("tmp");
    let run_dir = tmp.path().join("oci-runs").join("run-collision");
    std::fs::create_dir_all(&run_dir).expect("create run dir");
    // The first candidate collides with a pre-created file; the
    // bounded mint loop retries with a fresh candidate and succeeds.
    let colliding = run_dir.join("caduceus_env_collide.env");
    std::fs::write(&colliding, b"pre-existing").expect("seed collision");
    let file = OciEnvFile::create_with_names_for_tests(
        &run_dir,
        &env_map(&[("A", "1")]),
        &["collide".to_string()],
    )
    .expect("retry must succeed");
    assert_ne!(
        file.path(),
        colliding,
        "the mint must not reuse the colliding candidate"
    );
    assert!(file.path().exists());
}

#[test]
fn exhausted_mint_fails_with_typed_error() {
    let tmp = tempfile::tempdir().expect("tmp");
    let run_dir = tmp.path().join("oci-runs").join("run-exhaust");
    std::fs::create_dir_all(&run_dir).expect("create run dir");
    // Every candidate collides with the same seeded file: the bounded
    // mint is exhausted and fails with the typed unique-name error.
    let colliding = run_dir.join("caduceus_env_exhaust.env");
    std::fs::write(&colliding, b"pre-existing").expect("seed collision");
    let candidates: Vec<String> = std::iter::repeat_n("exhaust".to_string(), 64).collect();
    let err = OciEnvFile::create_with_names_for_tests(&run_dir, &env_map(&[]), &candidates)
        .expect_err("exhausted mint must fail");
    assert!(
        matches!(err, CaduceusError::Other(ref m) if m.contains("unique")),
        "expected a typed error naming the unique-name failure; got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Byte layout: sorted KEY=VALUE lines (design D5)
// ---------------------------------------------------------------------------

#[test]
fn bytes_are_exact_sorted_lines_with_trailing_newline() {
    let tmp = tempfile::tempdir().expect("tmp");
    // Deliberately out of insertion order: BTreeMap iteration must
    // produce the sorted layout.
    let file = OciEnvFile::create(
        &tmp.path().join("oci-runs").join("run-bytes"),
        &env_map(&[
            ("CADUCEUS_RUN_ID", "run-1"),
            ("ALPHA", "a-value"),
            ("CADUCEUS_RESULT_PATH", "/output/worker-result.json"),
            ("BETA", "b-value"),
            ("HOME", "/tmp"),
        ]),
    )
    .expect("create");
    let body = std::fs::read_to_string(file.path()).expect("read body");
    let expected = concat!(
        "ALPHA=a-value\n",
        "BETA=b-value\n",
        "CADUCEUS_RESULT_PATH=/output/worker-result.json\n",
        "CADUCEUS_RUN_ID=run-1\n",
        "HOME=/tmp\n",
    );
    assert_eq!(body, expected, "exact sorted KEY=VALUE byte layout");
}

// ---------------------------------------------------------------------------
// Deletion: Drop on every path (spec "Deletion on ... every path")
// ---------------------------------------------------------------------------

#[test]
fn drop_deletes_the_file() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path;
    {
        let file = OciEnvFile::create(
            &tmp.path().join("oci-runs").join("run-drop"),
            &env_map(&[("A", "1")]),
        )
        .expect("create");
        path = file.path().to_path_buf();
        assert!(path.exists());
        drop(file);
    }
    assert!(!path.exists(), "drop must delete the env file");
}

#[test]
fn deletion_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tmp");
    let file = OciEnvFile::create(
        &tmp.path().join("oci-runs").join("run-idem"),
        &env_map(&[("A", "1")]),
    )
    .expect("create");
    let path = file.path().to_path_buf();
    // Remove the file out from under the guard; the drop must not
    // fail (deletion is best-effort and idempotent).
    std::fs::remove_file(&path).expect("out-of-band removal");
    drop(file);
    assert!(!path.exists());
}

#[test]
fn drop_runs_on_unwind() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path;
    {
        let file = OciEnvFile::create(
            &tmp.path().join("oci-runs").join("run-unwind"),
            &env_map(&[("A", "1")]),
        )
        .expect("create");
        path = file.path().to_path_buf();
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _guard = file; // panic while the guard is alive
            panic!("simulated unwind");
        }));
        assert!(result.is_err(), "panic must propagate");
    }
    assert!(
        !path.exists(),
        "the guard must run on unwind and delete the env file"
    );
}

/// A newline-bearing value cannot be represented in the line-based
/// env-file format: rejected with a typed error BEFORE any file is
/// created.
#[test]
fn newline_value_rejected_before_file_creation() {
    let tmp = tempfile::tempdir().expect("tmp");
    let run_dir = tmp.path().join("oci-runs").join("run-newline");
    let env = env_map(&[("GOOD", "ok"), ("BAD", "line1\nline2")]);
    let err = OciEnvFile::create(&run_dir, &env).expect_err("must reject");
    match &err {
        CaduceusError::Config(msg) => {
            assert!(msg.contains("BAD"), "error must name the var: {msg}");
            assert!(
                !msg.contains("line1"),
                "error must never contain the value: {msg}"
            );
        }
        other => panic!("expected CaduceusError::Config; got: {other:?}"),
    }
    let entries = std::fs::read_dir(&run_dir).expect("run dir exists");
    assert_eq!(
        entries.count(),
        0,
        "no env file may exist after a rejection"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle guard: create-failure leaves no env file (spec R5)
// ---------------------------------------------------------------------------

struct FakeOciRunState;

impl OciRunState for FakeOciRunState {
    fn insert(&self, _row: &ContainerRunRow) -> CaduceusResult<()> {
        Ok(())
    }
    fn update_state(&self, _run_id: &str, _state: &OciLifecycleState) -> CaduceusResult<()> {
        Ok(())
    }
    fn list_pending_reconciliation(&self) -> CaduceusResult<Vec<ContainerRunRow>> {
        Ok(Vec::new())
    }
    fn get(&self, _run_id: &str) -> CaduceusResult<Option<ContainerRunRow>> {
        Ok(None)
    }
    fn delete(&self, _run_id: &str) -> CaduceusResult<()> {
        Ok(())
    }
}

/// Stub engine whose `create` always exits non-zero.
const FAILING_CREATE_STUB: &str = r#"#!/bin/sh
case "$1" in
  create) echo "stub: create failed" >&2; exit 1 ;;
  *) exit 0 ;;
esac
"#;

/// Drive the canonical lifecycle against the failing-create stub engine and
/// assert the env file is gone before the create error propagates.
#[tokio::test]
#[serial]
async fn create_failure_leaves_no_env_file() {
    let tmp = tempfile::tempdir().expect("tmp");
    let stub_dir = tempfile::tempdir().expect("stub tmp");
    let script = stub_dir.path().join("docker");
    std::fs::write(&script, FAILING_CREATE_STUB).expect("write stub");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");

    let run_dir = tmp.path().join("oci-runs").join("run-guard");
    let env_file = OciEnvFile::create(&run_dir, &env_map(&[("CADUCEUS_RUN_ID", "run-guard")]))
        .expect("create env file");
    let path: PathBuf = env_file.path().to_path_buf();
    assert!(path.exists(), "env file must exist before the lifecycle");

    // Prepend the stub dir to PATH for the duration of the call.
    let original_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        format!("{}:{original_path}", stub_dir.path().display()),
    );

    let cfg = Config::test_defaults(tmp.path());
    let spec = ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worktree: tmp.path().join("worktree"),
        run_id: "run-guard".to_string(),
        context_json: "{}".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: CancellationToken::new(),
        issue_title: "t".to_string(),
        issue_body: "b".to_string(),
        labels: Vec::new(),
        branch_name: "b".to_string(),
    };
    let argv = vec![
        "docker".to_string(),
        "create".to_string(),
        "--env-file".to_string(),
        path.display().to_string(),
        "image@sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    ];

    let state = Arc::new(FakeOciRunState);
    let worktree = cfg
        .workdir_base
        .join("owner")
        .join("repo")
        .join(&spec.run_id);
    let facts = support::runtime_facts(&cfg, &spec.run_id, &worktree);
    let resolved = resolve(cfg.sandbox(), &facts, &spec).expect("sandbox resolves");
    let adapter = oci_lifecycle::OciAdapter::new(
        SandboxEngine::Docker,
        state,
        cfg.state_dir.clone(),
        facts.daemon_id,
        spec.issue.clone(),
        spec.issue.display_key(),
        "test-command-sha".to_string(),
        argv,
        Some(env_file),
    );
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        oci_lifecycle::run_oci_lifecycle(
            &resolved,
            &adapter,
            &oci_lifecycle::LifecycleTimeouts::from_config(&cfg),
            CancellationToken::new(),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("lifecycle must not hang");

    // Restore PATH before asserting so a failure path cannot leak it.
    std::env::set_var("PATH", &original_path);

    match result {
        Err(CaduceusError::OciCreateFailed { .. }) => {}
        other => panic!("expected OciCreateFailed; got: {other:?}"),
    }
    assert!(
        !Path::new(&path).exists(),
        "env file must be deleted before the create result is propagated"
    );
}
