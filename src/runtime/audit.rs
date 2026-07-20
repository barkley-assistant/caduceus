//! Audit hook that enforces the "never auto-merge" contract.
//!
//! Every code path that would trigger a GitHub merge must route
//! through this module. The contract is absolute: the daemon
//! never calls the merge API; human review is required.

use crate::infra::error::{CaduceusError, CaduceusResult};

/// Refuse any request to enable auto-merge on a pull request.
///
/// This is the runtime defence for AC-04 ("Never auto-merge").
/// The grep-time evidence (`grep -RE '/pulls/.*/merge' src/`)
/// must return zero production hits; this function is the one
/// explicit refuse-list entry in the GitHub client.
pub fn refuse_auto_merge() -> CaduceusResult<()> {
    Err(CaduceusError::Other(
        "auto-merge is refused: human review is required for all PR merges \
  (CONTRACTS.md FINAL-001 AC-04)"
            .to_string(),
    ))
}
