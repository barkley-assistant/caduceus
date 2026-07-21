//! Trusted-host executor — wraps [`crate::worker::supervisor::supervise`].
//!
//! The [`TrustedHostExecutor`] implements [`Executor`] by delegating to the
//! 8-arg `supervise` free function, preserving its behaviour exactly.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use tokio_util::sync::CancellationToken;

use crate::executor::{Executor, ExecutorSpec};
use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::CaduceusResult;
use crate::worker::supervisor::{supervise, SupervisorOutcome};

/// Executor that dispatches workers on the trusted host via
/// [`crate::worker::supervisor::supervise`].
#[derive(Clone, Debug)]
pub struct TrustedHostExecutor {
    cfg: Config,
}

impl TrustedHostExecutor {
    /// Wrap a config snapshot.
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }
}

impl Executor for TrustedHostExecutor {
    fn run<'a>(
        &'a self,
        spec: &'a ExecutorSpec,
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<SupervisorOutcome>> + Send + 'a>> {
        let self_exe: &'a Path = &spec.self_exe;
        let cfg: &'a Config = &self.cfg;
        let issue: &'a IssueKey = &spec.issue;
        let worktree: &'a Path = &spec.worktree;
        let run_id: &'a str = &spec.run_id;
        let context_json: &'a str = &spec.context_json;
        let worker_command: &'a [String] = &spec.worker_command;
        let cancellation: CancellationToken = spec.cancellation.clone();

        Box::pin(async move {
            supervise(
                self_exe,
                cfg,
                issue,
                worktree,
                run_id,
                context_json,
                worker_command,
                cancellation,
            )
            .await
        })
    }
}
