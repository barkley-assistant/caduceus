//! Upgrade choice state machine for OCI executor isolation.
//!
//! [`UpgradeChoice`] is a tri-state decision persisted by the operator.
//! [`UpgradePolicy`] validates that a choice has been made before the
//! daemon starts in OCI mode.

use serde::{Deserialize, Serialize};

use crate::executor::ExecutorKind;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Tri-state upgrade decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeChoice {
    /// No choice has been made yet.
    NotAsked,
    /// Operator chose to upgrade to this mode.
    Chosen(ExecutorKind),
    /// Operator explicitly deferred the decision.
    Deferred,
}

/// Validates the upgrade choice at daemon startup.
#[derive(Clone, Debug)]
pub struct UpgradePolicy;

impl UpgradePolicy {
    /// Check if an upgrade choice has been persisted. If `None` at
    /// start, the daemon refuses to run.
    pub fn validate(choice: Option<UpgradeChoice>) -> CaduceusResult<()> {
        match choice {
            Some(UpgradeChoice::Chosen(_)) => Ok(()),
            Some(UpgradeChoice::Deferred) => Ok(()),
            Some(UpgradeChoice::NotAsked) | None => Err(CaduceusError::OciUpgradeChoiceRequired),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests live in `tests/executor/upgrade_test.rs`.
// ---------------------------------------------------------------------------
