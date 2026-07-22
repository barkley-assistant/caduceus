//! OCI lifecycle failure stubs for the failure matrix (AC-03).
//!
//! The pure-state OCI lifecycle test uses `ExecutorSpec` /
//! `IsolationPolicy` directly — no live container engine is
//! started. These helpers build the worker-script bodies and the
//! `ExecutorSpec` shapes needed to drive each stage of the
//! 5-step OCI lifecycle (create, start, wait, stop, remove) to a
//! typed error without requiring Docker/Podman.
//!
//! The fixtures here are intentionally tiny: they only exercise
//! the typed error mapping in
//! `src/executor/oci_lifecycle.rs::to_oci_error` and the
//! resulting `OciRunState` row. Real-engine behaviors live in
//! `tests/executor/oci_lifecycle_test.rs` and are gated by
//! `CADUCEUS_RUN_ISOLATION_TESTS`.

#![allow(dead_code)]

use std::path::PathBuf;

use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;

/// A worker script body that exits non-zero immediately —
/// drives the `create` stage of the OCI lifecycle to a
/// `OciCreateFailed` typed error.
pub fn failing_worker_script_body() -> &'static str {
    "#!/bin/sh\nexit 1\n"
}

/// Build an `ExecutorSpec` for a pure-state OCI lifecycle test
/// that has no live container engine behind it. The
/// `worker_command` points at a script that will fail, and the
/// `cancellation` token is fresh so the lifecycle runs to
/// completion of whichever step first surfaces an error.
pub fn spec_for_stage(run_id: &str, worker_script: PathBuf) -> ExecutorSpec {
    ExecutorSpec {
        self_exe: PathBuf::from("/usr/bin/caduceus"),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worktree: PathBuf::from("/tmp/worktree"),
        run_id: run_id.to_string(),
        context_json: r#"{"stage":"fail"}"#.to_string(),
        worker_command: vec![worker_script.to_string_lossy().to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
        network_profile: None,
    }
}
