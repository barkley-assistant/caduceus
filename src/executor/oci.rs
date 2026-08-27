//! OCI executor — dispatches workers via Docker or Podman CLI.
//!
//! The executor resolves a typed [`SandboxSpec`] from the sandbox
//! config and runtime facts, renders the `create` argv with the pure
//! renderer, then delegates to [`oci_lifecycle::run_with_argv`] for the
//! five-step container lifecycle (create → start → wait → stop →
//! remove). The state DAO is injected through the config's state
//! directory.
//!
//! The renderer is the sole argv producer in the crate: `resolve` owns
//! every host-path and identity decision, and `oci_lifecycle` only
//! consumes already-rendered argv.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::executor::sandbox_spec::{self, RuntimeFacts};
use crate::executor::{oci_lifecycle, sandbox_renderer, Executor, ExecutorSpec};
use crate::infra::config::Config;
use crate::infra::error::CaduceusResult;
use crate::state::oci_run::OciRunDao;
use crate::state::store;
use crate::worker::supervisor::SupervisorOutcome;

/// Host-side output directory for a run: the sibling `result` dir of
/// the worktree under the same parent (matches the legacy mount-path
/// derivation shape, but now at the fixed container path `/output`).
/// The worktree itself stays the single workspace mount — the
/// double-RW same-host bug is gone.
fn derive_output_dir(cfg: &Config, spec: &ExecutorSpec) -> PathBuf {
    spec.worktree
        .parent()
        .map(|parent| parent.join("result"))
        .unwrap_or_else(|| cfg.workdir_base.join("result"))
}

/// Executor that dispatches workers via Docker or Podman CLI.
#[derive(Clone, Debug)]
pub struct OciExecutor {
    cfg: Config,
}

impl OciExecutor {
    /// Wrap a config snapshot.
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }
}

impl Executor for OciExecutor {
    fn run<'a>(
        &'a self,
        spec: &'a ExecutorSpec,
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<SupervisorOutcome>> + Send + 'a>> {
        Box::pin(async move {
            // 1. Resolve the closed typed spec. All host-path,
            //    identity, and mount decisions happen here; the
            //    renderer invents nothing.
            let runtime = RuntimeFacts {
                run_id: spec.run_id.clone(),
                issue: spec.issue.clone(),
                worker_command: spec.worker_command.clone(),
                worktree: spec.worktree.clone(),
                output_dir: derive_output_dir(&self.cfg, spec),
                daemon_id: sandbox_spec::derive_daemon_id(&self.cfg),
                workdir_base: self.cfg.workdir_base.clone(),
            };
            let resolved = sandbox_spec::resolve(self.cfg.sandbox(), &runtime)?;

            // 2. Open the state database.
            let db_path = self.cfg.state_dir.join(store::DB_FILENAME);
            let conn = store::open(&db_path)?;
            let dao = OciRunDao::new(conn);

            // 3. Render the create argv. Secret env files are created
            //    after resolution (`secret_transport`) and passed as
            //    renderer parameters; today the daemon writes no
            //    secrets, so the slice is empty and no `--env-file`
            //    is emitted.
            let engine = self.cfg.sandbox().engine;
            let argv = sandbox_renderer::render_with_env_files(&resolved, engine, &[]);

            // 4. Run the lifecycle with the rendered argv.
            oci_lifecycle::run_with_argv(
                &self.cfg,
                spec,
                &dao,
                engine,
                argv,
                spec.cancellation.child_token(),
            )
            .await
        })
    }
}
