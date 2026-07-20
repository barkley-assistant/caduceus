//! Worker contract — env sanitisation, the worker command resolver,
//! the supervisor that runs the worker in its own Unix session, the
//! JSON context builder, and the prompt-template writer.
//!
//! This module is the single source of truth for everything the
//! daemon needs to spawn, supervise, and communicate with the worker
//! bridge. The supervisor is the only module that talks to the
//! worker; nothing else may import a crate that talks to a worker.
//!
//! After the `src/` restructure (issue #13), the pre-existing single
//! `worker.rs` file is preserved verbatim here as `worker_contract.rs`.
//! Its public surface (`WorkerResult`, `sanitized_env`,
//! `parse_result_file`, etc.) remains reachable at
//! `crate::worker::WorkerResult` and below.

pub mod context;
pub mod prompt;
pub mod supervisor;
pub mod worker_contract;

// Re-export the canonical worker-contract surface at `crate::worker::*`
// so callers that reach for `WorkerResult`, `parse_result_file`,
// `sanitized_env`, etc. resolve the same way they did before the
// restructure.
pub use crate::worker::worker_contract::{
    parse_result_file, sanitized_env, spawn, validate_worker_result, SanitizedEnvInputs,
    WorkerResult, WorkerStatus, DEFAULT_ALLOWLIST_EXACT, DEFAULT_ALLOWLIST_PREFIXES, MAX_ARTIFACTS,
    MAX_ARTIFACT_KEY_LEN, MAX_RESULT_FILE_BYTES, MAX_SUMMARY_BYTES, MAX_TITLE_BYTES,
};
