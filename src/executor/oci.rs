//! OCI executor — dispatches workers via Docker or Podman CLI.
//!
//! The executor runs the pre-flight probe (`engine_probe`) to collect
//! the runtime facts (worktree owner, `.git` type, engine mode) and
//! create the daemon-owned host artifacts, resolves a typed
//! [`SandboxSpec`] from the sandbox config and those facts, renders
//! the `create` argv with the pure renderer, then delegates to
//! [`oci_lifecycle::run_with_argv`] for the five-step container
//! lifecycle (create → start → wait → stop → remove). The state DAO
//! is injected through the config's state directory.
//!
//! The renderer is the sole argv producer in the crate: `resolve` owns
//! every host-path and identity decision, `engine_probe` is the sole
//! pre-flight I/O surface, and `oci_lifecycle` only consumes
//! already-rendered argv.

use std::future::Future;
use std::pin::Pin;

use crate::executor::{
    engine_probe, oci_lifecycle, sandbox_renderer, sandbox_spec, Executor, ExecutorSpec,
};
use crate::infra::config::Config;
use crate::infra::error::CaduceusResult;
use crate::state::oci_run::OciRunDao;
use crate::state::store;
use crate::worker::supervisor::SupervisorOutcome;

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
            // 1. Pre-flight probe: collect the runtime facts (worktree
            //    owner uid/gid, host `.git` type, engine mode) and
            //    create the daemon-owned host artifacts. Every
            //    unsupported-configuration refusal (typed
            //    `OciIdentityUnsupported`) is raised here — before any
            //    `create` argv exists, so `oci_lifecycle` is never
            //    reached on a refusal path.
            let runtime = engine_probe::probe_runtime_facts(&self.cfg, spec).await?;

            // 2. Resolve the closed typed spec. All host-path,
            //    identity, and mount decisions happen here; the
            //    renderer invents nothing.
            let resolved = sandbox_spec::resolve(self.cfg.sandbox(), &runtime)?;

            // 3. Open the state database.
            let db_path = self.cfg.state_dir.join(store::DB_FILENAME);
            let conn = store::open(&db_path)?;
            let dao = OciRunDao::new(conn);

            // 4. Render the create argv. Secret env files are created
            //    after resolution (`secret_transport`) and passed as
            //    renderer parameters; today the daemon writes no
            //    secrets, so the slice is empty and no `--env-file`
            //    is emitted.
            let engine = self.cfg.sandbox().engine;
            let argv = sandbox_renderer::render_with_env_files(&resolved, engine, &[]);

            // 5. Run the lifecycle with the rendered argv.
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
