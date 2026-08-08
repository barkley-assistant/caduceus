//! Integration tests for the supervisor dispatch protocol
//! (`src/worker/supervisor/dispatch.rs`). Moved out of the inline
//! `#[cfg(test)]` module per AGENTS.md.

use caduceus::worker_supervisor::{
    clear_heartbeat, decode_frame, encode_frame, open_transcript,
    parse_starttime_from_stat_for_tests, read_heartbeat, read_proc_starttime, truncate_transcript,
    verify_identity, write_heartbeat, BoundedTranscriptWriter, ControlFrame, WorkerRunPaths,
    MAX_FRAME_BYTES,
};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

#[test]
fn frame_round_trip() {
    let cases = vec![
        ControlFrame::Ready { pgid: 1234 },
        ControlFrame::Done {
            status: 0,
            signaled: false,
        },
        ControlFrame::Done {
            status: 9,
            signaled: true,
        },
        ControlFrame::Fatal {
            reason: "boom".to_string(),
        },
        ControlFrame::Terminate { force: false },
        ControlFrame::Terminate { force: true },
        ControlFrame::Ack,
    ];
    for case in cases {
        let encoded = encode_frame(&case).expect("encode");
        let (decoded, consumed) = decode_frame(&encoded).expect("decode");
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, case);
    }
}

#[test]
fn frame_rejects_wrong_version() {
    let mut bytes = encode_frame(&ControlFrame::Ack).expect("encode");
    // Mangle the version byte.
    bytes[6] = b'9';
    let err = decode_frame(&bytes).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("unsupported protocol version"), "{msg}");
}

#[test]
fn frame_rejects_oversize() {
    // Construct a buffer whose first 4 bytes encode a
    // length that exceeds MAX_FRAME_BYTES, then put enough
    // payload after it so the frame *appears* complete —
    // the decoder should reject it on the size check
    // before parsing the body.
    let mut bytes = Vec::new();
    let oversize = (MAX_FRAME_BYTES as u32) + 1;
    bytes.extend_from_slice(&oversize.to_le_bytes());
    bytes.resize(4 + oversize as usize, 0);
    let err = decode_frame(&bytes).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("exceeds cap"), "{msg}");
}

#[test]
fn heartbeat_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("hbeat");
    write_heartbeat(&path).expect("write");
    let read = read_heartbeat(&path).expect("read");
    assert!((chrono::Utc::now() - read).num_seconds().abs() < 5);
    clear_heartbeat(&path).expect("clear");
    assert!(read_heartbeat(&path).is_none());
}

#[test]
fn transcript_truncation_appends_marker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.log");
    let mut file = open_transcript(&path).expect("open");
    for _ in 0..1000 {
        file.write_all(b"chunk\n").expect("write");
    }
    drop(file);
    let truncated = truncate_transcript(&path, 64).expect("truncate");
    assert!(truncated);
    let meta = std::fs::metadata(&path).expect("stat");
    assert!(
        meta.len() <= 256,
        "transcript should be roughly capped; got {}",
        meta.len()
    );
    let body = std::fs::read_to_string(&path).expect("read");
    assert!(body.contains("truncated"), "marker missing from {body:?}");
}

#[test]
fn paths_ensure_dirs_creates_secure_layout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = WorkerRunPaths::new(dir.path().to_path_buf(), "RUN01".to_string());
    paths.ensure_dirs().expect("ensure_dirs");
    let meta = std::fs::metadata(dir.path().join("runs")).expect("stat runs");
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_writer_new_creates_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bw.log");
    let writer = BoundedTranscriptWriter::new(path.clone(), 1024).expect("new");
    assert!(path.is_file(), "file must exist");
    let meta = std::fs::metadata(&path).expect("stat");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "file mode must be 0600, got {:o}",
        meta.permissions().mode()
    );
    drop(writer);
}

#[test]
fn bounded_writer_under_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bw_under.log");
    let mut writer = BoundedTranscriptWriter::new(path.clone(), 1024).expect("new");
    let data = vec![b'a'; 100];
    writer.write_bytes(&data);
    assert!(!writer.truncated, "should not be truncated");
    writer.finalize().expect("finalize should succeed");
}

#[test]
fn bounded_writer_exact_fit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bw_exact.log");
    let mut writer = BoundedTranscriptWriter::new(path.clone(), 100).expect("new");
    let data = vec![b'a'; 100];
    writer.write_bytes(&data);
    assert!(!writer.truncated, "exact fit should not truncate");
    writer.finalize().expect("finalize should succeed");
}

#[test]
fn bounded_writer_over_limit_sets_truncated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bw_over.log");
    let mut writer = BoundedTranscriptWriter::new(path.clone(), 50).expect("new");
    let data = vec![b'a'; 100];
    writer.write_bytes(&data);
    assert!(writer.truncated, "should be truncated");
    let err = writer.finalize().expect_err("finalize should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("truncated"),
        "error must mention truncated, got {msg}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_writer_write_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bw_fail.log");
    let mut writer = BoundedTranscriptWriter::new(path.clone(), 1024).expect("new");
    // Write some bytes first.
    writer.write_bytes(b"first write");
    // Replace the file handle with /dev/full so writes fail.
    writer.file = std::fs::File::open("/dev/full").expect("open /dev/full");
    writer.write_bytes(b"this should fail");
    assert!(
        writer.write_failures > 0,
        "write_failures should be > 0, got {}",
        writer.write_failures
    );
    let err = writer.finalize().expect_err("finalize should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("write_failures"),
        "error must mention write_failures, got {msg}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_writer_truncation_takes_precedence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bw_prec.log");
    let mut writer = BoundedTranscriptWriter::new(path.clone(), 50).expect("new");
    // Write enough to trigger truncation.
    let data = vec![b'a'; 100];
    writer.write_bytes(&data);
    assert!(writer.truncated, "should be truncated");
    // Now replace file handle with /dev/full so further writes fail.
    writer.file = std::fs::File::open("/dev/full").expect("open /dev/full");
    writer.write_bytes(b"more data");
    assert!(
        writer.write_failures > 0,
        "write_failures should be > 0, got {}",
        writer.write_failures
    );
    let err = writer.finalize().expect_err("finalize should fail");
    let msg = format!("{err:?}");
    // Truncation takes precedence over write_failures.
    assert!(
        msg.contains("truncated"),
        "error must mention truncated, got {msg}"
    );
}

#[test]
fn bounded_writer_max_bytes_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bw_zero.log");
    let mut writer = BoundedTranscriptWriter::new(path.clone(), 0).expect("new");
    let data = vec![b'a'; 10];
    writer.write_bytes(&data);
    assert!(
        writer.truncated,
        "max_bytes=0: any write should set truncated"
    );
}
// later units use to verify a worker PID has not been reused before
// signalling. They are Linux-only because they read /proc/<pid>/stat.
#[cfg(target_os = "linux")]
#[test]
fn read_proc_starttime_parses_field22() {
    // Deterministic unit check of the field parser: feed a synthetic
    // /proc/<pid>/stat line and confirm field 22 (starttime) is read at
    // after-paren index 19.
    let synthetic =
        "1234 (fake_worker) S 1 1234 1234 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 12345678 0 0 0";
    assert_eq!(
        parse_starttime_from_stat_for_tests(synthetic),
        Some(12_345_678),
        "field 22 (0-based 19 after ')') must be the starttime"
    );

    // Integration check against a real, still-alive process. Spawn a
    // long-running child but never wait on it so it stays alive for the
    // read. /proc/<pid>/stat starttime is always non-zero for a live
    // process.
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id() as i32;
    let starttime = read_proc_starttime(pid);
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        matches!(starttime, Some(x) if x > 0),
        "live process starttime should be Some(>0), got {starttime:?}"
    );

    // A wildly impossible PID yields None (process gone).
    assert_eq!(read_proc_starttime(999_999), None);
}

#[cfg(target_os = "linux")]
#[test]
fn verify_identity_detects_reuse() {
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id() as i32;
    let starttime = read_proc_starttime(pid).expect("live starttime");
    assert!(starttime > 0);

    // Correct starttime → identity confirmed.
    assert!(
        verify_identity(pid, starttime),
        "matching starttime must verify"
    );
    // Off-by-one starttime → PID reuse / mismatch must reject.
    assert!(
        !verify_identity(pid, starttime + 1),
        "stale starttime must fail verification"
    );
    // Gone process → cannot verify.
    assert!(
        !verify_identity(999_999, 0),
        "missing process must fail verification"
    );

    let _ = child.kill();
    let _ = child.wait();
}
