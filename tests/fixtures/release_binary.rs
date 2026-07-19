//! Release binary fixture for Caduceus supervision tests.
//!
//! Provides a [`ReleaseBinary`] helper that locates the `caduceus`
//! binary (either from `CARGO_BIN_EXE_caduceus` or by walking up
//! from `current_exe()`), computes its SHA-256 digest, and
//! launches the `__worker-supervisor` hidden mode with controlled
//! arguments.
//!
//! The contract surface from `CONTRACTS.md`:
//!
//! * **CI-002** — fixtures MUST be hermetic and MUST NOT require
//!   production credentials. `ReleaseBinary` never reads a token
//!   and only accesses the local filesystem.
//! * **CI-004** — the supervisor binary is the same binary that
//!   ships in production, so `ReleaseBinary::locate` and
//!   `run_supervisor` exercise the same binary path the daemon
//!   uses.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The `__worker-supervisor` hidden command name.
pub const HIDDEN_COMMAND: &str = "__worker-supervisor";

/// Arguments for [`ReleaseBinary::run_supervisor`].
///
/// Mirrors the production `build_supervisor_command` signature
/// in `src/worker_supervisor.rs`.
#[derive(Clone, Debug)]
pub struct RunSupervisorArgs {
    /// Working tree path for the worker.
    pub worktree: PathBuf,
    /// Unique run identifier.
    pub run_id: String,
    /// Issue key in `owner/repo#number` format.
    pub issue: String,
    /// JSON context blob.
    pub context_json: String,
    /// Path to the transcript file.
    pub transcript: PathBuf,
    /// Path to the heartbeat file.
    pub heartbeat: PathBuf,
    /// Hard timeout in seconds.
    pub timeout_seconds: u64,
    /// Transcript max bytes before truncation.
    pub transcript_max_bytes: u64,
    /// Worker command and its arguments.
    pub worker: Vec<String>,
}

/// Locates the `caduceus` binary, computes its SHA-256 hash, and
/// launches supervised worker sessions.
pub struct ReleaseBinary;

impl ReleaseBinary {
    /// Locate the `caduceus` binary.
    ///
    /// First checks `std::env::var("CARGO_BIN_EXE_caduceus")` —
    /// cargo sets this for integration test binaries. Falls back
    /// to walking up from `current_exe()` like the existing
    /// `find_self_exe()` helpers in `worker_process_test.rs` and
    /// `worker_parent_death_test.rs`.
    pub fn locate() -> PathBuf {
        if let Ok(path) = std::env::var("CARGO_BIN_EXE_caduceus") {
            let pb = PathBuf::from(path);
            if pb.is_file() {
                return pb;
            }
        }
        // Fallback: walk up from current_exe()
        let mut here = std::env::current_exe().expect("current_exe");
        loop {
            if here.join("caduceus").is_file() {
                return here.join("caduceus");
            }
            if !here.pop() {
                panic!("could not find caduceus binary in target/debug");
            }
        }
    }

    /// Compute the SHA-256 hex digest of the file at `path`.
    pub fn sha256(path: &Path) -> String {
        use sha2::Digest;
        let data = std::fs::read(path).expect("read binary for sha256");
        let hash = sha2::Sha256::digest(&data);
        hex::encode(hash)
    }

    /// Launch the `caduceus __worker-supervisor` hidden command
    /// with the given arguments.
    ///
    /// The child's environment is cleared (`env_clear()`) and
    /// stdin/stdout/stderr are piped.
    ///
    /// Returns the [`Child`] handle. The caller must wait for it
    /// or kill it.
    pub fn run_supervisor(args: RunSupervisorArgs) -> Child {
        let exe = Self::locate();
        let mut cmd = Command::new(&exe);

        cmd.arg(HIDDEN_COMMAND);
        cmd.arg("--worktree").arg(&args.worktree);
        cmd.arg("--run-id").arg(&args.run_id);
        cmd.arg("--issue").arg(&args.issue);
        cmd.arg("--context-json").arg(&args.context_json);
        cmd.arg("--transcript").arg(&args.transcript);
        cmd.arg("--heartbeat").arg(&args.heartbeat);
        cmd.arg("--timeout").arg(args.timeout_seconds.to_string());
        cmd.arg("--transcript-max-bytes")
            .arg(args.transcript_max_bytes.to_string());
        for w in &args.worker {
            cmd.arg("--").arg(w);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.env_clear();

        cmd.spawn().expect("spawn supervisor")
    }
}
