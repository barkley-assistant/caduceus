//! Executor abstraction for worker dispatch.
//!
//! The [`Executor`] trait decouples worker dispatch from the concrete
//! trusted-host subprocess path. Dispatch sites call
//! [`executor_for_config`] to obtain an [`Arc<dyn Executor>`] matching
//! the configured mode, then call [`Executor::run`] with an
//! [`ExecutorSpec`].
//!
//! The module owns two implementations:
//!
//! * [`trusted_host::TrustedHostExecutor`] — wraps
//!   [`crate::worker::supervisor::supervise`] unchanged.
//! * [`oci::OciExecutor`] — stub returning
//!   [`CaduceusError::OciNotImplementedYet`]; filled in by Task 6.2.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::CaduceusResult;
use crate::worker::supervisor::SupervisorOutcome;

use self::oci::OciExecutor;
use self::trusted_host::TrustedHostExecutor;

// ---------------------------------------------------------------------------
// Submodules
// ---------------------------------------------------------------------------

pub mod oci;
pub mod oci_args;
pub mod secret_transport;
pub mod trusted_host;

// ---------------------------------------------------------------------------
// ExecutorSpec
// ---------------------------------------------------------------------------

/// Arguments to [`Executor::run`]. Every field the executor needs
/// to dispatch a worker, regardless of mode.
#[derive(Clone, Debug)]
pub struct ExecutorSpec {
    /// Path to the running caduceus binary (re-exec for supervisor mode).
    pub self_exe: PathBuf,
    /// The issue key being worked on.
    pub issue: IssueKey,
    /// The worktree root path (supervisor cwd; OCI volume mount target).
    pub worktree: PathBuf,
    /// Unique run identifier for this dispatch.
    pub run_id: String,
    /// JSON-encoded worker context.
    pub context_json: String,
    /// Worker command argv (bridge script + args).
    pub worker_command: Vec<String>,
    /// Cancellation token for daemon shutdown.
    pub cancellation: CancellationToken,
}

// ---------------------------------------------------------------------------
// ExecutorKind
// ---------------------------------------------------------------------------

/// Which execution mode the daemon uses to dispatch workers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    /// Default — subprocess-based dispatch on the host.
    #[default]
    TrustedHost,
    /// OCI container dispatch (seam for Task 6.2).
    Oci,
}

// ---------------------------------------------------------------------------
// Executor trait
// ---------------------------------------------------------------------------

/// Object-safe trait for worker dispatch.
///
/// Dispatch sites hold `Arc<dyn Executor>` and call `run(&spec).await`.
/// The trait is object-safe: no generic parameters, no `impl Future`
/// return — returns `Pin<Box<dyn Future>>` instead.
pub trait Executor: Send + Sync {
    /// Run the worker according to the configured execution mode.
    ///
    /// Returns a [`SupervisorOutcome`] on success or a typed
    /// [`CaduceusError`] on failure.
    fn run<'a>(
        &'a self,
        spec: &'a ExecutorSpec,
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<SupervisorOutcome>> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Construct the executor matching the configured mode.
///
/// Reads `cfg.executor_mode` and dispatches to the matching concrete
/// implementation. The factory is the single entry point used by
/// `Services::production`; tests inject their own `Arc<dyn Executor>`
/// via `Services::for_tests`.
///
/// `Oci` execution is allowed in config; the `OciExecutor` stub
/// returns `CaduceusError::OciNotImplementedYet` at `run` time. Task
/// 6.2 replaces the stub with the real OCI CLI lifecycle.
pub fn executor_for_config(cfg: &Config) -> Arc<dyn Executor> {
    match cfg.executor_mode {
        ExecutorKind::TrustedHost => Arc::new(TrustedHostExecutor::new(cfg.clone())),
        ExecutorKind::Oci => Arc::new(OciExecutor::new(cfg.clone())),
    }
}
