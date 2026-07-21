//! OCI executor — dispatches workers via Docker or Podman CLI.
//!
//! The executor delegates to [`oci_lifecycle::run`] for the five-step
//! container lifecycle (create → start → wait → stop → remove). The
//! state DAO is injected through the config's state directory.

use std::future::Future;
use std::pin::Pin;

use crate::executor::oci_lifecycle;
use crate::executor::{Executor, ExecutorSpec};
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
            // Open the state database.
            let db_path = self.cfg.state_dir.join(store::DB_FILENAME);
            let conn = store::open(&db_path)?;
            let dao = OciRunDao::new(conn);

            // Run the lifecycle.
            oci_lifecycle::run(&self.cfg, spec, &dao, spec.cancellation.child_token()).await
        })
    }
}
