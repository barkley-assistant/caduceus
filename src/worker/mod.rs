//! Worker contract — env sanitisation, the worker command resolver,
//! the supervisor that runs the worker in its own Unix session, the
//! JSON context builder, and the prompt-template writer.
//!
//! This module is the single source of truth for everything the
//! daemon needs to spawn, supervise, and communicate with the worker
//! bridge. The supervisor is the only module that talks to the
//! worker; nothing else may import a crate that talks to a worker.

pub mod context;
pub mod prompt;
pub mod supervisor;
pub mod worker_contract;

// Re-export the canonical worker-contract surface at `crate::worker::*`
// so callers that reach for `WorkerResult`, `parse_result_file`,
// `sanitized_env`, etc. resolve the same way.
pub use crate::worker::worker_contract::{
    parse_result_file, sanitized_env, spawn, validate_worker_result, SanitizedEnvInputs,
    WorkerResult, WorkerStatus, DEFAULT_ALLOWLIST_EXACT, DEFAULT_ALLOWLIST_PREFIXES, MAX_ARTIFACTS,
    MAX_ARTIFACT_KEY_LEN, MAX_RESULT_FILE_BYTES, MAX_SUMMARY_BYTES, MAX_TITLE_BYTES,
};

// Test seam: re-export the body-truncation helper so integration
// tests can assert the truncation contract without owning a runtime.
pub use self::context::truncate_body_for_tests;
