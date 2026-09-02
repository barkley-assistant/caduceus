//! Process-level lock-race test for `migrate-state --to-sqlite`.
//!
//! The migration subprocess is started with a large JSON state so it
//! holds the daemon lock for a measurable window. The parent polls
//! `DaemonLock::try_acquire` until it observes `None`, proving the
//! lock guard covers the migration I/O, then kills the child and
//! verifies the exit signal semantics.

use caduceus::queue::DaemonLock;
#[path = "fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn caduceus_binary() -> PathBuf {
    let mut exe = std::env::current_exe().expect("current exe");
    exe.pop(); // deps
    exe.pop(); // debug
    exe.push("caduceus");
    exe
}

fn write_huge_state(state_dir: &Path, count: usize) {
    let mut entries = String::from("\"entries\":{");
    for i in 0..count {
        let key = format!("owner/repo#{i}");
        let entry = format!(
            "{key:?}:{{\"phase\":\"queued\",\"ticket_type\":\"code\",\"attempts\":0,\
             \"queued_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}}"
        );
        entries.push_str(&entry);
        if i + 1 < count {
            entries.push(',');
        }
    }
    entries.push('}');
    fs::write(
        state_dir.join("state.json"),
        format!("{{\"version\":1,{entries}}}"),
    )
    .unwrap();
}

#[test]
fn migration_holds_daemon_lock_across_subprocess_io() {
    let root = tempdir("lock-race");
    let state_dir = root.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    write_huge_state(&state_dir, 50_000);

    let config_path = root.join("config.yaml");
    fs::write(
        &config_path,
        format!(
            r#"---
poll_interval_seconds: 120
state_dir: "{}"
state_backend: "json"
worker_command: ["python3", "bridge.py"]
reduced_containment_acknowledged: true
"#,
            state_dir.to_string_lossy()
        ),
    )
    .unwrap();

    let mut child = Command::new(caduceus_binary())
        .arg("migrate-state")
        .arg("--to-sqlite")
        .env("CADUCEUS_CONFIG", &config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn migration subprocess");

    // Poll for the subprocess to acquire the daemon lock. Drop any
    // lock we win immediately so the subprocess has a clear window.
    let start = Instant::now();
    let mut observed_contention = false;
    while start.elapsed() < Duration::from_secs(5) {
        let probe = DaemonLock::try_acquire(&state_dir).expect("lock probe");
        if let Some(_lock) = probe {
            // Lock is free; subprocess either hasn't started or
            // already finished. Drop and pause before retrying.
            drop(_lock);
            thread::sleep(Duration::from_millis(10));
        } else {
            observed_contention = true;
            break;
        }
    }

    assert!(
        observed_contention,
        "migration subprocess never held the daemon lock"
    );

    // The daemon lock prevents a second migration from running.
    let second_attempt = DaemonLock::try_acquire(&state_dir).expect("second lock probe");
    assert!(
        second_attempt.is_none(),
        "lock should still be held by migration"
    );

    // Kill the migration while it still holds the lock. The exit code
    // must NOT be 137 (handled separately via `signaled`).
    child.kill().expect("kill migration subprocess");
    let status = child.wait().expect("wait for child");

    // Explicit signal check per the safety rules for lock-race tests.
    assert!(
        status.signal().is_some(),
        "migration child should have been killed by SIGKILL"
    );
    assert_eq!(
        status.code().unwrap_or(1),
        1,
        "process killed by signal must yield code().unwrap_or(1) == 1"
    );
}
