use caduceus::worker::supervisor::process_lifecycle::TREE;

#[serial_test::serial]
#[test]
fn own_process_has_no_children_at_test_start() {
    assert!(TREE.list_children(std::process::id() as i32).is_empty());
}

#[serial_test::serial]
#[test]
fn adopting_own_process_is_idempotent() {
    TREE.adopt_subtree(std::process::id() as i32)
        .expect("first subtree adoption should be non-fatal");
    TREE.adopt_subtree(std::process::id() as i32)
        .expect("second subtree adoption should be non-fatal");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[serial_test::serial]
#[test]
fn list_children_returns_direct_children() {
    use std::process::{Command, Stdio};

    // `sleep 30 & sleep 30; wait` keeps two direct sleep children
    // alive under the shell PID for the duration of the test.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 30 & sleep 30; wait")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fixture shell");
    let shell_pid = child.id() as i32;

    // Let the shell exec the two sleeps so they appear as direct
    // children. 50 ms is plenty for fork+exec on both platforms.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let children = TREE.list_children(shell_pid);
    assert_eq!(
        children.len(),
        2,
        "expected two sleep children, got {children:?}"
    );

    // Ground-truth cross-check via pgrep where available.
    if let Ok(out) = Command::new("pgrep")
        .args(["-P", &shell_pid.to_string()])
        .output()
    {
        if out.status.success() {
            let expected: Vec<i32> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect();
            assert!(
                children.iter().all(|pid| expected.contains(pid)),
                "list_children returned PIDs not in pgrep ground truth: {children:?} vs {expected:?}"
            );
        }
    }

    // Best-effort cleanup; ignore kill errors (the test may have
    // raced a sleep exit).
    let _ = child.kill();
    let _ = child.wait();
}
