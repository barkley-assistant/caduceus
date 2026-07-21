//! Audit hook that enforces the "never auto-merge" contract.
//!
//! Every code path that would trigger a GitHub merge must route
//! through this module. The contract is absolute: the daemon
//! never calls the merge API; human review is required.
//!
//! Also provides circuit breaker audit hooks that emit structured
//! tracing events for circuit state transitions and NeedsAttention
//! escalation.

use tracing::{info, warn};

use crate::daemon::orchestration::Clock;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::scheduler::circuit::{CircuitState, ExhaustedEntry};

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

/// Record a circuit state transition.
///
/// Emits a `tracing::info!` event with scope, scope_id, the
/// transition path, and the current timestamp.
pub fn record_circuit_transition(
    scope: &str,
    scope_id: &str,
    from: &CircuitState,
    to: &CircuitState,
    clock: &dyn Clock,
) {
    info!(
        circuit.scope = %scope,
        circuit.scope_id = %scope_id,
        circuit.from = %from,
        circuit.to = %to,
        circuit.timestamp = clock.now_unix(),
        "circuit state transition"
    );
}

/// Emit a NeedsAttention event for a circuit that has been open
/// longer than the max degraded age.
pub fn emit_needs_attention(scope: &str, scope_id: &str, reason: &str, entry: &ExhaustedEntry) {
    warn!(
        circuit.scope = %scope,
        circuit.scope_id = %scope_id,
        circuit.reason = %reason,
        circuit.consecutive_failures = entry.consecutive_failures,
        circuit.last_failure_at = entry.last_failure_at,
        circuit.opened_at = entry.opened_at,
        "circuit needs attention: circuit has been open beyond max degraded age"
    );
}
