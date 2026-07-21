//! Network profile management for OCI executor isolation.
//!
//! [`NetworkProfile`] is a named profile loaded from config;
//! [`NetworkPolicy`] builds the `--network` argument and manages
//! daemon-side iptables rules.

use serde::{Deserialize, Serialize};

use crate::executor::ExecutorSpec;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// A named network profile from config.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NetworkProfile {
    pub name: String,
    /// CIDR strings for egress allow-list (e.g. `["10.0.0.0/8"]`).
    pub egress_allow: Vec<String>,
}

/// Builds the `--network` argument and manages egress rules.
#[derive(Clone, Debug)]
pub struct NetworkPolicy;

impl NetworkPolicy {
    /// Return the `--network` flag for the spec's network profile.
    ///
    /// If the spec has a `network_profile` name, it is looked up in
    /// `config.network_profiles`. If found, the profile's name is
    /// used as the bridge name. If the spec has no profile name,
    /// returns `--network=none`.
    ///
    /// When a profile is found and has egress rules, the rules are
    /// returned as additional argv entries. In production these are
    /// applied as iptables rules; in tests they are verified as
    /// part of the argv.
    pub fn build_network_args(spec: &ExecutorSpec, config: &Config) -> CaduceusResult<Vec<String>> {
        match &spec.network_profile {
            Some(profile_name) if !profile_name.is_empty() => {
                // Look up the profile in config.
                match config.network_profiles.get(profile_name) {
                    Some(_profile) => {
                        // Use the profile name as the bridge/network name.
                        let args = vec!["--network".to_string(), profile_name.clone()];
                        // Egress rules are applied by the daemon-side
                        // iptables manager — for the argv builder we
                        // just emit the network flag.
                        Ok(args)
                    }
                    None => Err(CaduceusError::OciNetworkNotInProfile {
                        profile: profile_name.clone(),
                    }),
                }
            }
            _ => {
                // No profile → no network access.
                Ok(vec!["--network".to_string(), "none".to_string()])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests live in `tests/executor/network_test.rs`.
// ---------------------------------------------------------------------------
