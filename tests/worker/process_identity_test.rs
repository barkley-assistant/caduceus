use caduceus::worker_supervisor::{read_proc_starttime, IDENTITY};

#[cfg(target_os = "macos")]
#[test]
fn macos_identity_start_ticks_is_nanosecond_scaled() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let start_ticks = IDENTITY
        .start_ticks(std::process::id() as i32)
        .expect("current process has a start timestamp");
    assert_eq!(start_ticks % 1_000, 0);

    let start_seconds = start_ticks / 1_000_000_000;
    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_secs();
    assert!(start_seconds > 0);
    assert!(start_seconds <= now_seconds);
    assert!(now_seconds - start_seconds < 24 * 60 * 60);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_identity_matches_the_free_function_seam() {
    let pid = std::process::id() as i32;
    assert_eq!(IDENTITY.start_ticks(pid), read_proc_starttime(pid));
    assert!(IDENTITY.is_alive(pid));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_identity_is_an_inert_stub() {
    let pid = std::process::id() as i32;
    assert_eq!(IDENTITY.start_ticks(pid), None);
    assert!(!IDENTITY.is_alive(pid));
}
