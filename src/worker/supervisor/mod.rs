//! Worker process supervision.
//!
//! This module owns the in-process supervisor that the daemon
//! uses to spawn and tear down the bridge. The contract is
//! pinned by `CONTRACTS.md` "Worker environment and result",
//! "Hermes plugin compatibility contract", and Task 5.1:
//!
//! * The public daemon never spawns the bridge directly. It
//!   re-execs the same `caduceus` binary in a hidden
//!   `__worker-supervisor` mode that owns the worker session.
//! * The supervisor and the daemon talk over a length-bounded,
//!   versioned control/status framing protocol carried over
//!   the supervisor's inherited `stdin` (daemon→supervisor)
//!   and `stdout` (supervisor→daemon) descriptors.
//! * The supervisor forks the worker behind an exec-gate pipe.
//!   The worker calls `setsid` but cannot `exec` until the
//!   supervisor confirms `READY(pgid)` and the daemon
//!   acknowledges it with `ACK`. If either side dies before
//!   `ACK`, the gate EOFs and the pre-exec child exits without
//!   running the harness.
//! * After `ACK`, unexpected supervisor exit makes the daemon
//!   kill the recorded session; daemon death closes the
//!   control pipe (stdin) and makes the live supervisor kill
//!   the worker session.
//! * On Linux, the supervisor calls
//!   `prctl(PR_SET_CHILD_SUBREAPER)` before spawning so any
//!   detached descendants are still reaped by the supervisor.
//!   Cleanup enumerates descendant PIDs from `/proc`, signals
//!   the original negative PGID plus every descendant, waits
//!   two seconds, rediscovers, sends `SIGKILL`, and reaps
//!   until no descendants remain.
//! * The supervisor only ever sees the cleared worker
//!   environment — daemon credentials never appear in any
//!   inherited descriptor or pipe frame.
//!
//! The hidden command is dispatched in [`crate::main`] (the
//! CLI host) before public command parsing.
//!
//! # Safety note
//!
//! The crate's `#![forbid(unsafe_code)]` policy forbids `unsafe`
//! blocks anywhere in the source tree. The supervisor needs to
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

#![allow(dead_code, unused_imports)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command as TokioCommand};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};

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
