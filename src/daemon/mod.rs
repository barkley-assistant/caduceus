//! The tick loop and orchestration — every surface that owns the
//! daemon's running state lives here.
//!
//! Module shape:
//!
//! - [`orchestration`] — the `Services` graph, `ActiveRunGuard`,
//!   `FailureClass`, and the production supervisor adapters.
//! - [`signals`] — Unix signal handling and the daemon's shutdown
//!   handshake.
//! - [`tick`] — the tick controller; the fan-out controller that
//!   imports from every other module.
//! - [`status`] — the `caduceus status` command surface.

pub mod orchestration;
pub mod signals;
pub mod status;
pub mod tick;

// Explicit re-exports — `tick` exposes the test seams `run_blocking`
// and `exit_code_for_tests` so they remain reachable at
// `crate::daemon::tick::*`.
