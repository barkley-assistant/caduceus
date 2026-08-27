//! Network argument construction for OCI executor isolation.
//!
//! [`NetworkPolicy`] builds the `--network` argument from
//! `config.sandbox().network`: `none` → `--network none`,
//! `unrestricted` → `--network host`. The named-profile machinery
//! (`NetworkProfile` / `egress_allow`) was removed with the prototype
//! `network_profiles` config surface.

use crate::executor::ExecutorSpec;
use crate::infra::config::{Config, SandboxNetwork};
use crate::infra::error::CaduceusResult;

/// Builds the `--network` argument from the sandbox network mode.
#[derive(Clone, Debug)]
pub struct NetworkPolicy;

impl NetworkPolicy {
    /// Return the `--network` flag for the configured sandbox network
    /// mode. The `spec` parameter is retained for call-site stability.
    pub fn build_network_args(
        _spec: &ExecutorSpec,
        config: &Config,
    ) -> CaduceusResult<Vec<String>> {
        let mode = match config.sandbox().network {
            SandboxNetwork::None => "none",
            SandboxNetwork::Unrestricted => "host",
        };
        Ok(vec!["--network".to_string(), mode.to_string()])
    }
}

// Tests live in `tests/executor/network_test.rs`.
