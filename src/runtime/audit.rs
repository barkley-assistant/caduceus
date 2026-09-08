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

/// DAR §13 deprecation audit event: an Investigation entry reached a
/// terminal and its outcome was archived. The structured event name
/// is a stable operator-facing contract (see
/// [`emit_investigation_archived`]).
pub const INVESTIGATION_ARCHIVED_EVENT: &str = "investigation_archived";

/// DAR §4.4 startup-reconcile audit event (issue #331, release N+1):
/// a non-terminal Investigation row was terminated by the reconcile
/// pass at store open. The structured event name is the
/// operator-visible outcome mandated by
/// docs/architecture/auto-review.md §4.4 (see
/// [`emit_review_migration_terminated_investigation`]).
pub const REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT: &str =
    "review_migration_terminated_investigation";

/// Queue-level admission audit event (issue #331, release N+1): a
/// `StateStore::enqueue` call tried to admit a new Investigation
/// entry and was rejected. Replaces the release-N deprecation warning
/// (`investigation_admitted_deprecated`).
pub const INVESTIGATION_ADMISSION_REJECTED_EVENT: &str = "investigation_admission_rejected";

/// Refuse any request to enable auto-merge on a pull request.
///
/// This is the runtime defence for the "Never auto-merge"
/// contract. The grep-time evidence (`grep -RE '/pulls/.*/merge'
/// src/`) must return zero production hits; this function is the
/// one explicit refuse-list entry in the GitHub client.
pub fn refuse_auto_merge() -> CaduceusResult<()> {
    Err(CaduceusError::Other(
        "auto-merge is refused: human review is required for all PR merges \
  (FINAL-001 AC-04; see src/state/checkpoints.rs)"
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

/// DAR §13 archive audit: an Investigation entry reached a terminal
/// and its outcome was archived. In N+1 (issue #331) this fires from
/// the startup reconcile pass (`source = "reconcile/startup"`) that
/// terminates non-terminal Investigation rows at store open. The
/// `source` parameter keeps the operator log self-describing without
/// duplicating the audit seam.
pub fn emit_investigation_archived(repo: &str, issue: u64, source: &str, outcome: &str) {
    info!(
        target: "caduceus",
        event = INVESTIGATION_ARCHIVED_EVENT,
        repo = repo,
        issue = issue,
        source = source,
        outcome = outcome,
        "investigation entry archived under deprecation (DAR §12)"
    );
}

/// DAR §4.4 startup reconcile (issue #331, release N+1): a
/// non-terminal Investigation row was terminated by the reconcile
/// pass at store open. `phase_before` is the phase the row carried
/// before termination, as a stable snake_case string. Pairs with
/// [`emit_investigation_archived`] (`source = "reconcile/startup"`)
/// so operators see both the archive and the termination outcome for
/// the same row.
pub fn emit_review_migration_terminated_investigation(repo: &str, issue: u64, phase_before: &str) {
    warn!(
        target: "caduceus",
        event = REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT,
        repo = repo,
        issue = issue,
        phase_before = phase_before,
        "investigation entry terminated by the startup reconcile pass \
         (DAR §4.4); the work never executed — re-file as an \
         auto_review or code ticket if still needed"
    );
}
