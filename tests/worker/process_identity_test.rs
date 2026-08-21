use caduceus::worker_supervisor::{read_proc_starttime, IDENTITY};

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
