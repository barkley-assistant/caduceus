#![allow(dead_code, unused_imports)]
use super::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::github::issue::IssueKey;
use crate::github::Client;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::scheduler::Pool;
use crate::state::queue::{ClaimToken, Phase, QueueEntry, StateStore};
use crate::worker::supervisor::SupervisorOutcome;
use crate::worktree::{GitRunner, Worktree};

// ---------------------------------------------------------------------------
// Services — the dependency-injection surface the orchestrator uses
// ---------------------------------------------------------------------------

/// Trait abstraction over wall-clock access. Production callers
/// use [`SystemClock`]; tests use a fake that returns deterministic
/// [`DateTime<Utc>`] values.
pub trait Clock: Send + Sync {
    /// The current wall-clock time in UTC.
    fn now(&self) -> DateTime<Utc>;

    /// The current Unix timestamp in seconds. Default impl delegates
    /// to `now().timestamp()` so existing implementors get it for free.
    fn now_unix(&self) -> i64 {
        self.now().timestamp()
    }
}

/// Production wall-clock adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Trait abstraction over the GitHub API client. Production callers
/// wrap [`Client`] in [`GithubClientAdapter`]; tests use a
/// `wiremock`-backed `Client` and pass it through the same adapter
/// so the contract surface is unchanged.
pub trait GithubClient: Send + Sync {
    /// Borrow the inner `Arc<Client>` so call sites that need
    /// the full surface (e.g. cached `Arc<HttpCache>` test
    /// seams) can still reach it without an extra trait method
    /// per call.
    fn inner(&self) -> &Arc<Client>;
}

/// Production GitHub adapter. Thin — every trait method would
/// just forward to the inner `Client`, so the trait exposes the
/// underlying reference once and lets the orchestrator call
/// [`Client`] methods directly. The adapter exists to make the
/// orchestrator's dependency injection explicit at every call
/// site: `services.github.inner().get(...)` reads as "go through
/// the GitHub dependency" rather than "go through a free function".
#[derive(Clone)]
pub struct GithubClientAdapter {
    client: Arc<Client>,
}

impl std::fmt::Debug for GithubClientAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubClientAdapter").finish()
    }
}

impl GithubClientAdapter {
    /// Wrap an `Arc<Client>`.
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }
}

impl GithubClient for GithubClientAdapter {
    fn inner(&self) -> &Arc<Client> {
        &self.client
    }
}

/// Trait abstraction over the [`GitRunner`]. Production callers
/// wrap a concrete `GitRunner` in [`GitRunnerAdapter`]; tests use
/// the same wrapper.
pub trait Git: Send + Sync {
    /// Borrow the underlying [`GitRunner`].
    fn runner(&self) -> &GitRunner;
}

/// Production git adapter.
#[derive(Clone)]
pub struct GitRunnerAdapter {
    runner: GitRunner,
}

impl std::fmt::Debug for GitRunnerAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRunnerAdapter").finish()
    }
}

impl GitRunnerAdapter {
    /// Wrap a [`GitRunner`].
    pub fn new(runner: GitRunner) -> Self {
        Self { runner }
    }
}

impl Git for GitRunnerAdapter {
    fn runner(&self) -> &GitRunner {
        &self.runner
    }
}

/// Bundle of dependencies the orchestrator needs for one tick.
/// The bundle is the canonical point of injection: production
/// constructs one of these per daemon process; tests construct
/// one per test with mock adapters.
///
/// `executor` is the `Arc<dyn Executor>` that the tick dispatch
/// site uses to spawn workers. The trait object seam is the
/// single point of dispatch — `TrustedHostExecutor` and
/// `OciExecutor` both implement the `Executor` trait.
#[derive(Clone)]
pub struct Services {
    pub clock: Arc<dyn Clock>,
    pub github: Arc<dyn GithubClient>,
    pub git: Arc<dyn Git>,
    pub executor: Arc<dyn crate::executor::Executor>,
    pub pool: Arc<Pool>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("clock", &"Arc<dyn Clock>")
            .field("github", &"Arc<dyn GithubClient>")
            .field("git", &"Arc<dyn Git>")
            .field("executor", &"Arc<dyn Executor>")
            .field("pool", &"Arc<Pool>")
            .finish()
    }
}

impl Services {
    /// Production convenience constructor. The `executor` is built
    /// via [`crate::executor::executor_for_config`] from the
    /// supplied `Config`; tests inject a custom executor via
    /// [`Services::for_tests`].
    pub fn production(
        cfg: &Config,
        clock: Arc<dyn Clock>,
        github: Arc<Client>,
        git: GitRunner,
        pool: Arc<Pool>,
    ) -> Self {
        let executor = crate::executor::executor_for_config(cfg);
        Self {
            clock,
            github: Arc::new(GithubClientAdapter::new(github)),
            git: Arc::new(GitRunnerAdapter::new(git)),
            executor,
            pool,
        }
    }

    /// Test-only constructor that takes pre-built trait objects so
    /// tests can mix real adapters with fakes. The `executor` is
    /// injected directly so tests can substitute mocks without
    /// touching the config layer.
    pub fn for_tests(
        clock: Arc<dyn Clock>,
        github: Arc<dyn GithubClient>,
        git: Arc<dyn Git>,
        executor: Arc<dyn crate::executor::Executor>,
        pool: Arc<Pool>,
    ) -> Self {
        Self {
            clock,
            github,
            git,
            executor,
            pool,
        }
    }
}
