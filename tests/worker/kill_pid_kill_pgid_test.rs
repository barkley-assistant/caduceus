#[cfg(unix)]
#[test]
fn kill_pid_terminates_a_trapping_shell() {
    use std::io::BufRead;
    use std::process::Command;
    use std::process::Stdio;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg("trap 'exit 0' TERM; echo READY; while :; do sleep 1; done")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn trapping shell");
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.take().expect("shell stdout"))
        .read_line(&mut ready)
        .expect("read shell readiness");
    assert_eq!(ready.trim(), "READY");
    let pid = child.id() as i32;

    caduceus::worker_supervisor::kill_pid(pid, 15);

    let status = child.wait().expect("wait for trapping shell");
    assert_eq!(status.code(), Some(0));
}
