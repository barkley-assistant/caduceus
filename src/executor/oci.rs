//! OCI executor stub — returns not-yet-implemented.
//!
//! This is the seam Task 6.2 fills in. The struct and impl exist so
//! `executor_for_config(ExecutorKind::Oci)` returns a working
//! `Arc<dyn Executor>` whose `run` immediately reports
//! `CaduceusError::OciNotImplementedYet`. The error message names
//! Task 6.2 so operators can navigate to the unblocking work.

use std::future::Future;
use std::pin::Pin;

use crate::executor::{Executor, ExecutorSpec};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::worker::supervisor::SupervisorOutcome;

/// Executor stub for OCI container dispatch. The `cfg` field is
/// retained so the struct surface matches `TrustedHostExecutor` and
/// Task 6.2 can fill the impl without changing the trait seam.
#[derive(Clone, Debug)]
pub struct OciExecutor {
    #[allow(dead_code)]
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
        _spec: &'a ExecutorSpec,
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<SupervisorOutcome>> + Send + 'a>> {
        Box::pin(async move { Err(CaduceusError::OciNotImplementedYet) })
    }
}
