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

    // Spawn two direct sleep children of this process. Keeping them as
    // direct children (no intermediate shell) makes cleanup deterministic:
    // kill+wait each and no orphan can reparent here to poison the
    // own_process_has_no_children_at_test_start assertion.
    let mut s1 = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep child 1");
    let mut s2 = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep child 2");
    let p1 = s1.id() as i32;
    let p2 = s2.id() as i32;

    // Let both fully exec before enumerating.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let children = TREE.list_children(std::process::id() as i32);
    assert!(
        children.contains(&p1) && children.contains(&p2),
        "expected both sleep children {p1},{p2}, got {children:?}"
    );

    // Ground-truth cross-check via pgrep where available.
    if let Ok(out) = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string()])
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

    // Deterministic cleanup: kill+wait each direct child. Nothing is
    // orphaned, so no reparenting can race the sibling assertion.
    let _ = s1.kill();
    let _ = s1.wait();
    let _ = s2.kill();
    let _ = s2.wait();
}
