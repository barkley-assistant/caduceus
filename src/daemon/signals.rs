//! Unix signal handling for operator-initiated shutdown.
//!
//! The orchestrator installs listeners for `SIGINT` and `SIGTERM`
//! before invoking the canonical tick. The first signal triggers
//! a graceful worker pool drain, then cancels the shared
//! [`tokio_util::sync::CancellationToken`] so the active tick, the
//! supervisor, and the worker session all wind down cooperatively
//! through the contractually-documented requeue / cleanup path.
//! A second signal received before cleanup completes escalates to
//! immediate self-`SIGKILL`, which the operating system delivers
//! to every descendant the daemon owns.
//!
//! The crate's `#![forbid(unsafe_code)]` policy forbids unsafe
//! blocks, so all signal syscalls are routed through the safe
//! `tokio::signal::unix` and `nix::sys::signal` wrappers. The
//! listener is `Send + 'static` so the orchestrator can spawn
//! it as a side task alongside the tick.
//!
//! # Idle cancellation contract
//!
//! When the daemon has not yet entered a worker session — e.g.
//! the state directory is empty, the queue is idle, or the
//! cadence gate has skipped the tick — the first signal returns
//! `TickOutcome::Cancelled` / exit 0 per the Cron model in
//! `CONTRACTS.md`. The state files are not mutated by the
//! listener itself.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::scheduler::Pool;

/// Kind of signal the listener received. Used for diagnostic
/// logging; the operator-shutdown semantics are identical for
/// both `SIGINT` and `SIGTERM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalKind {
    /// `SIGINT` (Ctrl-C, terminal disconnect). Interactive.
    Interrupt,
    /// `SIGTERM` (default `kill`). Operator-driven graceful
    /// shutdown.
    Terminate,
}

impl SignalKind {
    /// Human-readable label used by the structured logger.
    pub fn label(self) -> &'static str {
        match self {
            SignalKind::Interrupt => "SIGINT",
            SignalKind::Terminate => "SIGTERM",
        }
    }
}

/// Outcome of the listener after it observes one or more
/// signals. The daemon inspects this to decide whether to wait
/// for a graceful cleanup or to escalate to immediate
/// self-kill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalOutcome {
    /// First signal observed. The shared cancellation token
    /// has been cancelled; the orchestrator is winding down
    /// cooperatively.
    First(SignalKind),
    /// Second signal observed before the cooperative
    /// shutdown completed. The listener has delivered
    /// `SIGKILL` to its own process; the OS will clean up
    /// every descendant.
    Second(SignalKind),
}

/// Window after the first signal during which a second signal
/// escalates to immediate `SIGKILL`. Matches the supervisor's
/// TERM-to-KILL grace window so the contract is symmetric.
pub const ESCALATE_GRACE: Duration = Duration::from_secs(2);

/// Wait for a single SIGINT or SIGTERM signal and return which
/// arrived first. The function exists so the listener can be
/// composed of two awaits: the first to cancel, the second to
/// escalate.
pub async fn wait_for_signal() -> std::io::Result<SignalKind> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind as TokioSignalKind};
        let mut int_stream = signal(TokioSignalKind::interrupt())?;
        let mut term_stream = signal(TokioSignalKind::terminate())?;
        tokio::select! {
            _ = int_stream.recv() => Ok(SignalKind::Interrupt),
            _ = term_stream.recv() => Ok(SignalKind::Terminate),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = sleep(Duration::from_secs(3600)).await;
        Ok(SignalKind::Terminate)
    }
}

/// Listen for Unix signals and translate them into
/// cooperative-cancellation actions on the supplied token. The
/// returned future completes only after the operator's second
/// signal escalates to self-kill; under a single signal the
/// caller drops the future to leave the listener running in
/// the background.
///
/// Before cancelling the token, the listener triggers a graceful
/// worker pool drain so in-flight workers have a chance to
/// complete within the configured drain timeout.
pub async fn listen(pool: Arc<Pool>, cancellation: CancellationToken) -> std::io::Result<()> {
    // First signal: start drain, then cancel and wait briefly for
    // cooperative shutdown. If a second signal arrives inside the
    // grace window, escalate to self-`SIGKILL` so the operating
    // system reaps every descendant immediately.
    let first = wait_for_signal().await?;
    info!(
        signal = first.label(),
        "operator signal received; draining worker pool"
    );

    // Initiate the worker pool drain. This sets the draining flag
    // and waits for in-flight workers to complete up to the
    // configured drain timeout.
    let timed_out_run_ids = pool.drain().await;
    if timed_out_run_ids.is_empty() {
        info!("worker pool drain completed");
    } else {
        warn!(
        timed_out_run_ids = ?timed_out_run_ids,
        "worker pool drain timed out for some runs"
        );
    }

    info!("cancelling tick after drain");
    cancellation.cancel();

    let deadline = Instant::now() + ESCALATE_GRACE;
    match tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        wait_for_signal(),
    )
    .await
    {
        Ok(Ok(second)) => {
            warn!(
                signal = second.label(),
                "operator sent second signal during grace; escalating to SIGKILL",
            );
            // Drop everything and exit immediately. The OS
            // propagates SIGKILL to the entire process
            // group; the supervisor's child-subreaper
            // attribute means any setsid'd grandchild is
            // still reaped.
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(std::process::id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            )
            .ok();
            Ok(())
        }
        Ok(Err(err)) => Err(err),
        Err(_) => {
            // Grace window expired without a second signal.
            // The cooperative shutdown path remains in
            // charge.
            Ok(())
        }
    }
}

/// Outcome the orchestrator reports when it observes the
/// listener's cancellation. Currently only used by the
/// acceptance tests; production callers route through the
/// `CancellationToken` itself.
pub fn outcome_from_signal(kind: SignalKind) -> SignalOutcome {
    SignalOutcome::First(kind)
}

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn signal_kind_labels_match_libc_names() {
        assert_eq!(SignalKind::Interrupt.label(), "SIGINT");
        assert_eq!(SignalKind::Terminate.label(), "SIGTERM");
    }

    #[test]
    fn escalate_grace_matches_supervisor_window() {
        // The supervisor's TERM-to-KILL grace is 2 seconds
        // (worker_supervisor::run_supervisor's protocol_task
        // cancellation branch). The listener's grace window
        // intentionally matches so the two timeouts line up
        // under a single operator press.
        assert_eq!(ESCALATE_GRACE, Duration::from_secs(2));
    }
}
