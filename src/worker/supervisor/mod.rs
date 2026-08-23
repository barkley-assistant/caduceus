//! Worker process supervision.
//!
//! This module owns the in-process supervisor that the daemon
//! uses to spawn and tear down the bridge. The contract is
//! pinned by the worker-result contract in `src/worker/worker_contract.rs`.
//!
//! * The public daemon never spawns the bridge directly. It
//!   re-execs the same `caduceus` binary in a hidden
//!   `__worker-supervisor` mode that owns the worker session.
//! * The supervisor and the daemon talk over a length-bounded,
//!   versioned control/status framing protocol carried over
//!   the supervisor's inherited `stdin` (daemon→supervisor)
//!   and `stdout` (supervisor→daemon) descriptors.
//! * The supervisor `setsid`s and sends `READY(pgid)` *before*
//!   forking the worker. It blocks on the daemon's `ACK`; only
//!   after `ACK` does it spawn the worker. If the daemon dies
//!   (EOF on stdin) or sends a non-`ACK` frame before `ACK`,
//!   the supervisor exits without ever spawning a worker — so
//!   no orphaned worker process can exist before the PGID is
//!   confirmed.
//! * After `ACK`, unexpected supervisor exit makes the daemon
//!   kill the recorded session; daemon death closes the
//!   control pipe (stdin) and makes the live supervisor kill
//!   the worker session.
//! * Descendant cleanup: on Linux the supervisor marks itself a child
//!   subreaper with `prctl(PR_SET_CHILD_SUBREAPER)` before spawning the
//!   worker (wired in `process_lifecycle.rs` via `set_child_subreaper`),
//!   so orphaned grandchildren reparent here. Non-Linux platforms have
//!   no subreaper analogue: `procctl(PROC_REAP_ACQUIRE)` is FreeBSD-only.
//!   Teardown first signals the worker's original negative PGID, then
//!   enumerates descendants that `setsid`-ed out of the process group
//!   through the portable `TREE.list_children` seam — `collect_descendants`
//!   over `/proc` on Linux, `proc_listchildpids` on macOS, and a
//!   compile-time failure on unsupported platforms.
//!
//!   Each descendant is TERM-killed, a two-second grace sleep follows,
//!   then a bounded rediscover loop (3 iterations at 100 ms each)
//!   re-enumerates `TREE.list_children` and SIGKILLs any survivors
//!   (`dispatch.rs`). The pass is best-effort and bounded so a runaway
//!   descendant cannot stall teardown.
//!
//! * The supervisor only ever sees the cleared worker
//!   environment — daemon credentials never appear in any
//!   inherited descriptor or pipe frame.
//!
//! The hidden command is dispatched in [`crate::main`] (the
//! CLI host) before public command parsing.
//!
//! # Safety note
//!
//! The crate's `#![deny(unsafe_code)]` policy forbids `unsafe`
//! blocks outside the narrowly scoped macOS boot-time FFI
//! helper. The supervisor needs to
//! hand FDs across exec and to call `pipe2` / `setsid` /
//! `killpg`. Where the safe `nix` crate provides a wrapper
//! (`setsid`, `killpg`, `kill`, `pipe2`, `set_child_subreaper`),
//! the supervisor uses it directly. For the few operations
//! that have no safe wrapper in `nix` 0.29 (`OwnedFd` adoption
//! for tokio's async I/O, `prctl`), the supervisor uses
//! safe APIs only and routes the dangerous syscalls through
//! `tokio::process::Command::stdin/stdout/stderr(Stdio::piped())`
//! so the inherited-FD contract is satisfied without explicit
//! `unsafe`.

// Submodule declarations and re-exports. These preserve the historical
// `crate::worker::supervisor` public surface.

pub mod dispatch;
pub mod framing;
pub mod heartbeat;
pub mod outcome_transcript;
pub mod process_lifecycle;

use self::dispatch::*;
use self::framing::*;
use self::heartbeat::*;
use self::outcome_transcript::*;
use self::process_lifecycle::*;

pub use dispatch::*;
pub use framing::*;
pub use heartbeat::*;
pub use outcome_transcript::*;
pub use process_lifecycle::*;

// Test seam: re-export the synthetic-stat parser so integration
// tests can assert the field-22 contract without owning a runtime.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub use self::process_lifecycle::parse_starttime_from_stat_for_tests;

#[doc(hidden)]
pub use self::process_lifecycle::{decide_deadline_kill, DeadlineKillDecision};
