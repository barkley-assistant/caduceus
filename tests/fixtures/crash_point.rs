//! Crash point fixture for Caduceus supervision tests.
//!
//! Provides a [`CrashPoint`] helper that spawns a bash script,
//! watches its stdout for a marker line, and sends a configurable
//! signal when the marker appears. This lets tests simulate
//! deterministic crash points (SIGKILL, SIGABRT, SIGTERM) during
//! worker execution.
//!
//! The contract surface from `CONTRACTS.md`:
//!
//! * **CI-002** — fixtures MUST be hermetic and MUST NOT require
//!   production credentials. `CrashPoint` never reads a token,
//!   never touches a network interface.
//! * **CI-004** — the supervisor's signal handling is exercised
//!   through `CrashPoint`'s marker-driven kill/abort/term paths.

#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

/// Owns a temporary directory and provides helpers that spawn bash
/// scripts, watch for a stdout marker, and send a configurable
/// signal at the marker point.
pub struct CrashPoint {
    _dir: TempDir,
    workdir: PathBuf,
}

impl CrashPoint {
    /// Create a new `CrashPoint` under a unique tempdir with the
    /// given `label`.
    pub fn new(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("caduceus-crash-{label}-"))
            .tempdir()
            .expect("tempdir create");
        let workdir = dir.path().join("work");
        fs::create_dir_all(&workdir).expect("create workdir");
        Self { _dir: dir, workdir }
    }

    /// Path to the working directory owned by this fixture.
    pub fn workdir(&self) -> &PathBuf {
        &self.workdir
    }

    /// Write `script` to a file, spawn it under bash, watch stdout
    /// for the line containing `marker`, and send `SIGKILL` to the
    /// child process when the marker is seen.
    ///
    /// Returns `(exit_code, signaled)` where `signaled` is `true`
    /// if the process died by signal, and `exit_code` is the raw
    /// exit status (or negative signal number if signaled, depending
    /// on platform encoding).
    ///
    /// If the child exits before the marker appears, the function
    /// returns the child's exit code without sending a signal.
    pub fn kill_at_marker(&self, script: &str, marker: &str) -> (i32, bool) {
        self.run_marker(script, marker, SignalAction::Kill)
    }

    /// Like [`kill_at_marker`] but sends `SIGABRT`.
    pub fn abort_at_marker(&self, script: &str, marker: &str) -> (i32, bool) {
        self.run_marker(script, marker, SignalAction::Abort)
    }

    /// Like [`kill_at_marker`] but sends `SIGTERM`.
    /// SIGTERM results in exit code 143 (128 + 15).
    pub fn nonexit_at_marker(&self, script: &str, marker: &str) -> (i32, bool) {
        self.run_marker(script, marker, SignalAction::Term)
    }

    /// Internal: write script, spawn, watch stdout for marker,
    /// send the configured signal.
    fn run_marker(&self, script: &str, marker: &str, action: SignalAction) -> (i32, bool) {
        let script_path = self.workdir.join(format!("crash-{}.sh", rand_id()));
        let mut f = fs::File::create(&script_path).expect("create script");
        f.write_all(script.as_bytes()).expect("write script");
        drop(f);
        let mut perms = script_path.metadata().expect("stat").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");

        let mut child = Command::new("bash")
            .arg(&script_path)
            .current_dir(&self.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bash");

        let child_pid = child.id() as i32;

        // Read stdout in a thread looking for the marker.
        let stdout = child.stdout.take().expect("take stdout");
        let marker_owned = marker.to_string();
        let (tx, rx) = mpsc::channel::<bool>();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.contains(&marker_owned) {
                    let _ = tx.send(true);
                    return;
                }
            }
            // EOF without marker
            let _ = tx.send(false);
        });

        // Wait for marker signal or child exit
        let marker_found = rx.recv_timeout(Duration::from_secs(10));

        if let Ok(true) = marker_found {
            // Send the configured signal
            let sig = action.to_nix_signal();
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(child_pid), sig);
        }

        // Wait for the child to exit
        let status = child.wait().expect("wait for child");
        let signaled = status.code().is_none();
        let code = status.code().unwrap_or(-1);
        (code, signaled)
    }
}

enum SignalAction {
    Kill,
    Abort,
    Term,
}

impl SignalAction {
    fn to_nix_signal(&self) -> nix::sys::signal::Signal {
        match self {
            SignalAction::Kill => nix::sys::signal::Signal::SIGKILL,
            SignalAction::Abort => nix::sys::signal::Signal::SIGABRT,
            SignalAction::Term => nix::sys::signal::Signal::SIGTERM,
        }
    }
}

fn rand_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
