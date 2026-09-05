//! Trusted-host executor — wraps [`crate::worker::supervisor::supervise`].
//!
//! The [`TrustedHostExecutor`] implements [`Executor`] by delegating to the
//! 8-arg `supervise` free function, preserving its behaviour exactly.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use tokio_util::sync::CancellationToken;

use crate::executor::{Executor, ExecutorOutcome, ExecutorSpec, WorkTarget};
use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::worker::supervisor::supervise;
use crate::worker::worker_contract::WORKER_RESULT_FILE;

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
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<ExecutorOutcome>> + Send + 'a>> {
        let self_exe: &'a Path = &spec.self_exe;
        let cfg: &'a Config = &self.cfg;
        let worktree: &'a Path = &spec.worktree;
        let run_id: &'a str = &spec.run_id;
        let context_json: &'a str = &spec.context_json;
        let worker_command: &'a [String] = &spec.worker_command;
        let cancellation: CancellationToken = spec.cancellation.clone();

        // Issue targets flow through the supervisor's issue-path flags.
        // PR review targets are refused here until the supervisor
        // boundary carries a `WorkTarget` (issue #346) — never faked
        // into an issue.
        let (issue_key, issue_title, issue_body, labels, branch_name): (
            &'a IssueKey,
            &'a str,
            &'a str,
            &'a [String],
            &'a str,
        ) = match &spec.target {
            WorkTarget::Issue(issue) => (
                &issue.key,
                issue.title.as_str(),
                issue.body.as_str(),
                issue.labels.as_slice(),
                issue.branch_name.as_str(),
            ),
            WorkTarget::PullRequest(_) => {
                return Box::pin(async move {
                    Err(CaduceusError::Other(
                        "PR review targets are not yet supported on the trusted host"
                            .to_string(),
                    ))
                });
            }
        };

        Box::pin(async move {
            let outcome = supervise(
                self_exe,
                cfg,
                issue_key,
                worktree,
                run_id,
                context_json,
                worker_command,
                cancellation,
                issue_title,
                issue_body,
                labels,
                branch_name,
            )
            .await?;
            Ok(ExecutorOutcome {
                outcome,
                result_path: spec.worktree.join(WORKER_RESULT_FILE),
            })
        })
    }
}
