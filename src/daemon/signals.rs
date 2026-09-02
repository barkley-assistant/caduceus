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
//! The crate's `#![deny(unsafe_code)]` policy forbids unsafe
//! blocks, so all signal syscalls are routed through the safe
//! `tokio::signal::unix` and `nix::sys::signal` wrappers. The
//! listener is `Send + 'static` so the orchestrator can spawn
//! it as a side task alongside the tick.
//!
//! # Startup mask discipline (issue #270)
//!
//! A SIGINT/SIGTERM delivered between process start and handler
//! installation hits the default disposition and kills the daemon
//! (`ExitStatus(unix_wait_status(15))`). `run_blocking` therefore
//! blocks both signals on the orchestrator thread before the tokio
//! runtime exists ([`block_idle_signals`]), so the signal pends
//! instead of terminating the process. Runtime worker threads inherit
//! the blocked mask; each restores its own mask via a
//! `tokio::runtime::Builder::on_thread_start` hook gated on the
//! handlers-installed flag ([`unblock_worker_after_handlers_installed`]),
//! so worker subprocesses never inherit a blocked `SIGTERM`.
//! Inside `block_on`, [`install_idle_handlers`] eagerly registers both
//! tokio streams before the tick arm is polled — handler installation
//! therefore always precedes the tick, and the readiness marker
//! (`repos/mirrors`) is a valid "runtime is live" signal again.
//! The orchestrator thread restores its mask immediately after
//! registration ([`unblock_idle_signals`]); a signal that was pending
//! is then delivered to the now-installed handler, producing the
//! usual graceful cancel / exit 0.
//!
//! # Idle cancellation contract
//!
//! When the daemon has not yet entered a worker session — e.g.
//! the state directory is empty, the queue is idle, or the
//! cadence gate has skipped the tick — the first signal returns
//! `TickOutcome::Cancelled` / exit 0 per the Cron model. The
//! state files are not mutated by the
//! listener itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal as NixSignal};
use tokio::signal::unix::{signal, Signal as TokioSignal, SignalKind as TokioSignalKind};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::scheduler::Pool;

/// Set once [`install_idle_handlers`] has registered the tokio signal
/// streams. Runtime worker threads are spawned with SIGINT/SIGTERM
/// blocked (they inherit the orchestrator thread's pre-runtime mask)
/// and spin on this flag in
/// [`unblock_worker_after_handlers_installed`] before restoring their
/// own mask, so the whole process stays protected until the handlers
/// exist and worker subprocesses never inherit a blocked SIGTERM.
static IDLE_HANDLERS_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Drop guard that releases runtime worker threads blocked in
/// [`unblock_worker_after_handlers_installed`] even when the `block_on`
/// closure bails out before [`install_idle_handlers`] ran (registration
/// error or panic), so `Runtime::drop` never deadlocks on the workers.
pub(crate) struct WakeWorkersGuard;

impl Drop for WakeWorkersGuard {
    fn drop(&mut self) {
        IDLE_HANDLERS_INSTALLED.store(true, Ordering::Release);
    }
}

/// Block `SIGINT` and `SIGTERM` on the calling thread. Called before
/// the tokio runtime is built so every worker thread the runtime
/// spawns inherits the blocked mask; a signal delivered before the
/// handlers are installed then pends at the process level instead of
/// hitting the default disposition and killing the daemon (issue
/// #270). Idempotent — repeated calls are no-ops. The caller restores
/// the mask after registration via [`unblock_idle_signals`] (orchestrator
/// thread) / [`unblock_worker_after_handlers_installed`] (worker
/// threads).
pub fn block_idle_signals() -> std::io::Result<()> {
    let mut set = SigSet::empty();
    set.add(NixSignal::SIGINT);
    set.add(NixSignal::SIGTERM);
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None)?;
    Ok(())
}

/// Unblock `SIGINT` and `SIGTERM` on the calling thread. Called by the
/// orchestrator thread immediately after [`install_idle_handlers`]
/// registers the tokio streams, so a signal that was pending during
/// the blocked startup window is delivered to the now-installed
/// handler (graceful cancel / exit 0), never to the default
/// disposition. Only the two idle signals are touched; any other
/// blocked signals are preserved.
pub fn unblock_idle_signals() -> std::io::Result<()> {
    let mut set = SigSet::empty();
    set.add(NixSignal::SIGINT);
    set.add(NixSignal::SIGTERM);
    pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&set), None)?;
    Ok(())
}

/// Callback installed via `tokio::runtime::Builder::on_thread_start`.
/// Runtime worker threads are spawned with SIGINT/SIGTERM blocked
/// (inherited from the orchestrator thread). They wait until
/// [`install_idle_handlers`] has installed the tokio handlers, then
/// restore their own mask so the worker subprocesses they later spawn
/// never inherit a blocked SIGTERM (supervisor TERM-to-KILL contract).
pub(crate) fn unblock_worker_after_handlers_installed() {
    while !IDLE_HANDLERS_INSTALLED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    let mut set = SigSet::empty();
    set.add(NixSignal::SIGINT);
    set.add(NixSignal::SIGTERM);
    let _ = pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&set), None);
}

/// Eagerly register the SIGINT/SIGTERM tokio streams. Must be called
/// from inside the runtime (`signal` requires the signal driver);
/// handler installation is synchronous, so once this returns the
/// OS-level handlers exist and any signal that was pending because of
/// the pre-runtime mask is delivered to them. Sets the worker-wake
/// flag so the runtime worker threads can restore their own masks.
pub fn install_idle_handlers() -> std::io::Result<(TokioSignal, TokioSignal)> {
    let int_stream = signal(TokioSignalKind::interrupt())?;
    let term_stream = signal(TokioSignalKind::terminate())?;
    IDLE_HANDLERS_INSTALLED.store(true, Ordering::Release);
    Ok((int_stream, term_stream))
}

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
        let mut int_stream = signal(TokioSignalKind::interrupt())?;
        let mut term_stream = signal(TokioSignalKind::terminate())?;
        wait_for_signal_on(&mut int_stream, &mut term_stream).await
    }
    #[cfg(not(unix))]
    {
        let _ = sleep(Duration::from_secs(3600)).await;
        Ok(SignalKind::Terminate)
    }
}

/// Wait for a single SIGINT or SIGTERM on already-registered streams.
/// [`run_blocking`](crate::daemon::tick::run_blocking) registers the
/// streams eagerly via [`install_idle_handlers`] and passes them to
/// [`listen_on`]; the no-arg [`wait_for_signal`] constructs fresh
/// streams for the acceptance-test seam.
pub async fn wait_for_signal_on(
    int_stream: &mut TokioSignal,
    term_stream: &mut TokioSignal,
) -> std::io::Result<SignalKind> {
    #[cfg(unix)]
    {
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
    #[cfg(unix)]
    {
        let int_stream = signal(TokioSignalKind::interrupt())?;
        let term_stream = signal(TokioSignalKind::terminate())?;
        listen_on(pool, cancellation, int_stream, term_stream).await
    }
    #[cfg(not(unix))]
    {
        let _ = sleep(Duration::from_secs(3600)).await;
        Ok(())
    }
}

/// Like [`listen`] but drives the already-registered streams returned
/// by [`install_idle_handlers`]. The daemon's `run_blocking` uses this
/// path so handler installation is guaranteed to precede the tick arm;
/// the public [`listen`] delegates here after constructing its own
/// streams.
pub async fn listen_on(
    pool: Arc<Pool>,
    cancellation: CancellationToken,
    mut int_stream: TokioSignal,
    mut term_stream: TokioSignal,
) -> std::io::Result<()> {
    // First signal: start drain, then cancel and wait briefly for
    // cooperative shutdown. If a second signal arrives inside the
    // grace window, escalate to self-`SIGKILL` so the operating
    // system reaps every descendant immediately.
    let first = wait_for_signal_on(&mut int_stream, &mut term_stream).await?;
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
        wait_for_signal_on(&mut int_stream, &mut term_stream),
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
