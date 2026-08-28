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
//! * [`oci::OciExecutor`] — dispatches workers via Docker or Podman
//!   CLI. The `create` argv is produced by the pure
//!   [`sandbox_spec::resolve`] → [`sandbox_renderer::render`] pipeline;
//!   the five-step lifecycle lives in [`oci_lifecycle`].

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

pub mod engine_probe;
pub mod oci;
pub mod oci_lifecycle;
pub mod sandbox_renderer;
pub mod sandbox_spec;
pub mod secret_transport;
pub mod trusted_host;

pub use sandbox_spec::{
    EngineMode, GitShadowKind, MountSpec, RuntimeFacts, SandboxEngine, SandboxSpec,
};

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
    /// Issue title (UTF-8, NUL-free, may contain newlines).
    pub issue_title: String,
    /// Issue body (UTF-8, NUL-free, may contain newlines).
    pub issue_body: String,
    /// Label names (e.g. `["🤖 auto-fix"]`).
    pub labels: Vec<String>,
    /// Daemon-owned expected branch name.
    pub branch_name: String,
}

/// Result of [`Executor::run`] plus the host-side path the worker
/// result JSON was written to. `result_path` is mode-dependent
/// (`<worktree>/worker-result.json` for TrustedHost,
/// `<state_dir>/oci-runs/<run_id>/output/worker-result.json` for
/// OCI). The daemon reads the result exclusively from this path so
/// the tick loop stays engine-agnostic.
#[derive(Clone, Debug)]
pub struct ExecutorOutcome {
    pub outcome: SupervisorOutcome,
    pub result_path: PathBuf,
}

/// Which execution mode the daemon uses to dispatch workers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    /// Default — subprocess-based dispatch on the host.
    #[default]
    TrustedHost,
    /// OCI container dispatch.
    Oci,
}

/// Object-safe trait for worker dispatch.
///
/// Dispatch sites hold `Arc<dyn Executor>` and call `run(&spec).await`.
/// The trait is object-safe: no generic parameters, no `impl Future`
/// return — returns `Pin<Box<dyn Future>>` instead.
pub trait Executor: Send + Sync {
    /// Run the worker according to the configured execution mode.
    ///
    /// Returns an [`ExecutorOutcome`] — the [`SupervisorOutcome`]
    /// plus the host-side path the worker result JSON was written
    /// to — on success, or a typed [`CaduceusError`] on failure.
    fn run<'a>(
        &'a self,
        spec: &'a ExecutorSpec,
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<ExecutorOutcome>> + Send + 'a>>;
}

/// Construct the executor matching the configured mode.
///
/// Reads `cfg.executor_mode` and dispatches to the matching concrete
/// implementation. The factory is the single entry point used by
/// `Services::production`; tests inject their own `Arc<dyn Executor>`
/// via `Services::for_tests`.
pub fn executor_for_config(cfg: &Config) -> Arc<dyn Executor> {
    match cfg.executor_mode {
        ExecutorKind::TrustedHost => Arc::new(TrustedHostExecutor::new(cfg.clone())),
        ExecutorKind::Oci => Arc::new(OciExecutor::new(cfg.clone())),
    }
}
