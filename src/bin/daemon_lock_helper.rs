//! Small helper binary used by `tests/daemon_lock_test.rs` to
//! exercise the daemon-wide lock from a separate process.
//!
//! Modes:
//!
//! * `try-once <lock-path>` — take the lock once and print `WON`
//!   on success or `LOST: <reason>` on contention. Exit 0 either
//!   way so the parent test can detect both outcomes from
//!   stdout/stderr.
//! * `hold-and-release <lock-path> <millis>` — take the lock,
//!   sleep for the given number of milliseconds, then release.
//!
//! The binary lives under `src/bin/` so `cargo` discovers it
//! automatically and `env!("CARGO_BIN_EXE_daemon_lock_helper")` in
//! the integration tests resolves to its compiled path.

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use caduceus::DaemonLock;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mode = match args.next() {
        Some(arg) => arg.to_string_lossy().to_string(),
        None => return usage(),
    };
    let lock_path = match args.next() {
        Some(arg) => PathBuf::from(arg),
        None => return usage(),
    };
    let state_dir = lock_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    match mode.as_str() {
        "try-once" => try_once(&state_dir),
        "hold-and-release" => {
            let millis: u64 = match args
                .next()
                .and_then(|s| s.to_str().and_then(|s| s.parse().ok()))
            {
                Some(n) => n,
                None => return usage(),
            };
            hold_and_release(&state_dir, millis)
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: daemon_lock_helper try-once <lock-path>\n       daemon_lock_helper hold-and-release <lock-path> <millis>"
    );
    ExitCode::from(2)
}

fn try_once(state_dir: &std::path::Path) -> ExitCode {
    match DaemonLock::try_acquire(state_dir) {
        Ok(Some(lock)) => {
            println!("WON");
            // Hold the lock long enough for a sibling helper
            // (spawned in the same test) to attempt and lose.
            // 200ms is comfortably larger than the test's
            // process-spawn latency on slow CI hosts.
            thread::sleep(Duration::from_millis(200));
            drop(lock);
            ExitCode::from(0)
        }
        Ok(None) => {
            println!("LOST: held");
            ExitCode::from(0)
        }
        Err(err) => {
            println!("LOST: {err:?}");
            ExitCode::from(1)
        }
    }
}

fn hold_and_release(state_dir: &std::path::Path, millis: u64) -> ExitCode {
    let lock = match DaemonLock::try_acquire(state_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("could not acquire lock");
            return ExitCode::from(2);
        }
        Err(err) => {
            eprintln!("acquire error: {err:?}");
            return ExitCode::from(1);
        }
    };
    thread::sleep(Duration::from_millis(millis));
    drop(lock);
    ExitCode::from(0)
}
