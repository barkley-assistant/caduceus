//! Executable-script fixture helper.
//!
//! Tests that drive the engine adapter (or a PATH stub) need an
//! executable script written moments before the code-under-test execs
//! it. On Linux that write->exec sequence races the kernel's
//! `deny_write_access()` check inside `execve()`: the inode's writer
//! refcount (`i_writecount`) is dropped late in the close path
//! (`__fput`), so a concurrent `fork()`+`execve()` in another test
//! thread can transiently observe the freshly-closed file as
//! still-open-for-writing and fail the exec with `ETXTBSY` ("Text file
//! busy"). `fsync()` does NOT close the window — verified 2026-09-12
//! on `oci_provenance_test`: the fixture's `sync_all()` workaround
//! still flaked 20/60 isolated runs under 4 test threads. Retrying the
//! first exec until one succeeds closes it: a successful exec proves
//! the writer refcount has been released, and that one-time close
//! window never reopens for the file.

// Not every test binary that includes `fixtures/mod.rs` uses the helper;
// dead-code allowances match the other fixture modules.
#![allow(dead_code)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Marker argument the pre-flight exec passes so scripts that log
/// invocations (`$*`) can be identified in call logs.
const PREFLIGHT_ARG: &str = "__caduceus_fixture_preflight__";

/// Bounded retry: the transient ETXTBSY window clears in
/// microseconds-to-milliseconds; 20 attempts x 10ms far exceeds the
/// observed worst case while keeping the failure path diagnostic.
const MAX_PREFLIGHT_ATTEMPTS: usize = 20;
const PREFLIGHT_RETRY_DELAY_MS: u64 = 10;

/// Write an executable POSIX shell script under `dir` and prove it can
/// be exec'd before returning.
///
/// The caller's code execs the returned path immediately; the pre-flight
/// exec guarantees no subsequent exec of this file can hit the
/// transient `ExecutableFileBusy` (ETXTBSY) race described above.
pub fn write_executable_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("create script");
    file.write_all(body.as_bytes()).expect("write script");
    // Flush + close before chmod/exec so the write fd is fully released
    // (the fsync is not sufficient on its own — see module docs — but
    // keeps the file durable for hosts that do report in-flight-write
    // exec failures).
    file.sync_all().expect("sync script");
    drop(file);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
    preflight_exec(&path, name);
    path
}

/// Retry a single exec of the freshly-written script until it succeeds.
///
/// Exit status is irrelevant (scripts fall through to their default
/// branch for the marker arg); only a successful spawn matters, because
/// it proves the kernel's writer-refcount close-window has closed.
fn preflight_exec(path: &Path, name: &str) {
    let mut attempts = 0usize;
    loop {
        match Command::new(path)
            .arg(PREFLIGHT_ARG)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(_) => return,
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                attempts += 1;
                if attempts >= MAX_PREFLIGHT_ATTEMPTS {
                    panic!(
                        "spawn fixture script {name}: still ExecutableFileBusy \
                         after {MAX_PREFLIGHT_ATTEMPTS} attempts"
                    );
                }
                std::thread::sleep(Duration::from_millis(PREFLIGHT_RETRY_DELAY_MS));
            }
            Err(err) => panic!("spawn fixture script {name}: {err}"),
        }
    }
}
