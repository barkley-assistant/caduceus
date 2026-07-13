//! Typed error hierarchy for the daemon. The `CaduceusError` enum and the
//! `CaduceusResult` alias are the sole canonical error surface — they are
//! re-exported from `lib.rs` so call sites use one named alias.
//!
//! Variants are pinned in `CONTRACTS.md` under "Error contract". Task 1.5
//! adds code that constructs each variant. The map to POSIX exit codes
//! (used by the CLI router) is below.

use std::path::PathBuf;

/// All Caduceus errors. The variant set is normative; new errors always go
/// through a `CaduceusError` constructor, never a panic on external data.
#[derive(thiserror::Error, Debug)]
pub enum CaduceusError {
    #[error("caduceus configuration error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error("git {operation} failed: {stderr}")]
    Git {
        operation: &'static str,
        stderr: String,
    },

    #[error("GitHub API status {status}: {message}")]
    GitHubApi { status: u16, message: String },

    #[error("GitHub rate limited; reset in {reset_at}s (remaining {remaining}/{limit:?})")]
    RateLimited {
        reset_at: u64,
        remaining: u32,
        limit: Option<u32>,
    },

    #[error("failed to resolve a GitHub token: {0}")]
    TokenResolution(String),

    #[error("worker supervision failure: {0}")]
    Worker(String),

    #[error("worktree failure: {0}")]
    Worktree(String),

    #[error("queue failure: {0}")]
    Queue(String),

    #[error("corrupt state file at {}: {message}", path.display())]
    StateCorrupt { path: PathBuf, message: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

/// Canonical `Result` alias used everywhere in the daemon.
pub type CaduceusResult<T> = Result<T, CaduceusError>;

impl CaduceusError {
    /// Map a daemon error to the process exit code announced in
    /// `CONTRACTS.md` under "CLI contract".
    ///
    /// `run` returns 0 for processed/idle/concurrent/cadence/rate-limit/cancelled,
    /// 1 for configuration/corruption/invariant/pipeline failures. `status`
    /// returns 2 for missing state, 1 for corrupt state. CLI host code is
    /// responsible for selecting the right exit-code policy per command.
    pub fn exit_code(&self) -> std::process::ExitCode {
        use std::process::ExitCode;
        match self {
            CaduceusError::Cancelled => ExitCode::from(0),
            CaduceusError::RateLimited { .. } => ExitCode::from(0),
            _ => ExitCode::from(1),
        }
    }
}
