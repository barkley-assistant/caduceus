//! Regression tests for supervisor frame-parser defects 3 and 4
//! of #94 (#127, #128).
//!
//! * #127 — the reader must validate the frame length against
//!   `MAX_FRAME_BYTES` *before* allocating the body buffer, so a
//!   hostile 4-byte header claiming `len = 0xFFFFFFFF` cannot
//!   trigger a multi-GB allocation.
//! * #128 — the 4-byte header must be read with `read_exact` (via
//!   a peek-then-fill pattern) so a partial 1/2/3-byte header is
//!   rejected as `UnexpectedEof` instead of being parsed as a
//!   length prefix.
//!
//! These are pure unit tests over `std::io::Cursor` / tokio
//! `duplex` streams — no supervisor process is spawned, so the
//! fixtures module is not needed.

use std::io::Cursor;

use caduceus::worker_supervisor::{
    decode_frame, encode_frame, read_frame_sync, ControlFrame, MAX_FRAME_BYTES,
};

fn sync_read(bytes: &[u8]) -> std::io::Result<Option<ControlFrame>> {
    let mut cursor = Cursor::new(bytes.to_vec());
    let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
    let frame = read_frame_sync(&mut cursor, &mut buf)?;
    assert!(
        buf.len() <= MAX_FRAME_BYTES + 4,
        "buffer must stay bounded, got len {}",
        buf.len()
    );
    Ok(frame)
}

#[test]
fn oversized_header_does_not_allocate() {
    // A 4-byte header claiming `len = 0xFFFFFFFF` (~4 GiB). The
    // reader must reject it *before* allocating the body.
    let mut cursor = Cursor::new(vec![0xFF, 0xFF, 0xFF, 0xFF]);
    let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
    let err = read_frame_sync(&mut cursor, &mut buf).expect_err("oversize header must be rejected");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidData,
        "oversize length must surface as InvalidData, got {err:?}"
    );
    assert!(
        err.to_string().contains("exceeds cap"),
        "error must mention the cap, got {err:?}"
    );
    // Only the 4 header bytes were consumed — no body read (and no
    // body allocation) was attempted.
    assert_eq!(cursor.position(), 4, "no body bytes may be consumed");
    // The caller-owned buffer was never grown past its initial
    // capacity, proving the ~4 GiB allocation was skipped.
    assert!(
        buf.len() <= MAX_FRAME_BYTES + 4,
        "buffer must stay bounded, got len {}",
        buf.len()
    );
}

#[test]
fn partial_header_returns_unexpected_eof() {
    // 1, 2, and 3-byte "headers" followed by EOF must all be
    // rejected as `UnexpectedEof`, never parsed as a length prefix.
    for n in 1..=3 {
        let mut bytes = vec![0u8; n];
        bytes[0] = 0xFF;
        let mut cursor = Cursor::new(bytes);
        let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
        let err =
            read_frame_sync(&mut cursor, &mut buf).expect_err("partial header must be rejected");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "{n}-byte header must surface as UnexpectedEof, got {err:?}"
        );
        assert_eq!(
            cursor.position() as usize,
            n,
            "{n}-byte header: all available bytes consumed"
        );
    }
}

#[test]
fn clean_eof_returns_none() {
    // A peer that closes the pipe without sending anything is a
    // clean EOF (`Ok(None)`), not an error.
    let frame = sync_read(&[]).expect("clean EOF is not an error");
    assert!(frame.is_none(), "expected Ok(None), got {frame:?}");
}

#[test]
fn valid_frame_round_trips() {
    let cases = vec![
        ControlFrame::Ready { pgid: 4242 },
        ControlFrame::Done {
            status: 0,
            signaled: false,
        },
        ControlFrame::Fatal {
            reason: "boom".to_string(),
        },
        ControlFrame::Terminate { force: false },
        ControlFrame::Terminate { force: true },
        ControlFrame::Ack,
    ];
    for case in cases {
        let bytes = encode_frame(&case).expect("encode");
        let frame = sync_read(&bytes).expect("read").expect("a frame");
        assert_eq!(frame, case, "round-trip mismatch");
    }
}

#[test]
fn frame_at_max_bytes_round_trips() {
    // A frame whose payload is exactly MAX_FRAME_BYTES must pass
    // through the reader's `len > MAX_FRAME_BYTES` gate and
    // decode_frame's own check (the gate uses the same strict `>`
    // comparator as the decoder — no off-by-one).
    let max_payload = MAX_FRAME_BYTES;
    let mut bytes = Vec::with_capacity(4 + max_payload);
    bytes.extend_from_slice(&(max_payload as u32).to_le_bytes());
    // `FATAL` takes the rest of the line as its reason, so padding
    // with spaces keeps the frame decodable at the exact cap.
    let mut line = "v1 FATAL".to_string().into_bytes();
    line.resize(max_payload, b' ');
    bytes.extend_from_slice(&line);

    let mut cursor = Cursor::new(bytes.clone());
    let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
    let frame = read_frame_sync(&mut cursor, &mut buf).expect("max-size frame must be accepted");
    assert!(frame.is_some(), "max-size frame must decode");
    assert_eq!(cursor.position() as usize, 4 + max_payload);
    // The decoder agrees on the same bound.
    let (_, consumed) = decode_frame(&bytes).expect("decode_frame must accept the same frame");
    assert_eq!(consumed, 4 + max_payload);
}

#[test]
fn buffer_is_reused_across_frames() {
    // The caller-owned buffer must be reused (clear + resize, not
    // fresh allocation) so the hot stdin-killer loop does not
    // allocate per frame.
    let first = encode_frame(&ControlFrame::Ack).expect("encode");
    let second = encode_frame(&ControlFrame::Terminate { force: true }).expect("encode");
    let mut cursor = Cursor::new([first, second].concat());
    let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
    let a = read_frame_sync(&mut cursor, &mut buf)
        .expect("read")
        .expect("frame");
    let b = read_frame_sync(&mut cursor, &mut buf)
        .expect("read")
        .expect("frame");
    assert_eq!(a, ControlFrame::Ack);
    assert_eq!(b, ControlFrame::Terminate { force: true });
    assert!(
        buf.capacity() <= MAX_FRAME_BYTES + 4,
        "capacity must be reused"
    );
}

#[tokio::test]
async fn async_oversize_header_rejects_before_alloc() {
    use tokio::io::{AsyncWriteExt, DuplexStream};
    let (mut writer, mut reader): (DuplexStream, DuplexStream) =
        tokio::io::duplex(MAX_FRAME_BYTES + 8);
    writer
        .write_all(&[0xFF, 0xFF, 0xFF, 0xFF])
        .await
        .expect("write");
    // Close the writer; a buggy reader that allocated first would
    // then block forever awaiting the ~4 GiB body.
    drop(writer);
    let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
    let err = caduceus::worker_supervisor::read_frame_async(&mut reader, &mut buf)
        .await
        .expect_err("oversize header must be rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("exceeds cap"), "{msg}");
    assert!(buf.len() <= MAX_FRAME_BYTES + 4, "buffer must stay bounded");
}

#[tokio::test]
async fn async_partial_header_returns_error_not_none() {
    use tokio::io::{AsyncWriteExt, DuplexStream};
    let (mut writer, mut reader): (DuplexStream, DuplexStream) = tokio::io::duplex(16);
    writer.write_all(&[0x01]).await.expect("write");
    drop(writer);
    let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
    let err = caduceus::worker_supervisor::read_frame_async(&mut reader, &mut buf)
        .await
        .expect_err("partial header must be an error, not clean EOF");
    let msg = format!("{err:?}");
    assert!(msg.contains("short read on header"), "{msg}");
}

#[tokio::test]
async fn async_clean_eof_returns_none() {
    use tokio::io::DuplexStream;
    let (writer, mut reader): (DuplexStream, DuplexStream) = tokio::io::duplex(16);
    drop(writer);
    let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
    let frame = caduceus::worker_supervisor::read_frame_async(&mut reader, &mut buf)
        .await
        .expect("clean EOF is not an error");
    assert!(frame.is_none(), "expected Ok(None), got {frame:?}");
}
