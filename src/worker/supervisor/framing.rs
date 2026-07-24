#![allow(dead_code, unused_imports)]
use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command as TokioCommand};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// Control pipe protocol
// ---------------------------------------------------------------------------

/// Frame sent between the supervisor and the daemon over the
/// inherited `stdin`/`stdout` descriptors. The serialisation
/// is deliberately trivial: a 4-byte little-endian length
/// prefix followed by a UTF-8 string. The first line is the
/// version + opcode; the rest is opcode-specific payload text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlFrame {
    /// Supervisor announces that the worker has called
    /// `setsid` and recorded its PGID. Payload: the PGID as a
    /// decimal string.
    Ready { pgid: i32 },
    /// Supervisor announces the worker exited and the session
    /// is reaped. Payload: the exit code as a decimal string,
    /// or `signal:<n>` if it died by signal.
    Done { status: i32, signaled: bool },
    /// Supervisor encountered a fatal error before the worker
    /// could even start.
    Fatal { reason: String },
    /// Daemon tells the supervisor to terminate the worker.
    /// Payload: empty for SIGTERM, `kill` for SIGKILL after a
    /// 2-second grace period.
    Terminate { force: bool },
    /// Daemon confirms it has recorded the PGID and the worker
    /// may now `exec` the harness.
    Ack,
}

impl ControlFrame {
    pub fn opcode(&self) -> &'static str {
        match self {
            ControlFrame::Ready { .. } => "READY",
            ControlFrame::Done { .. } => "DONE",
            ControlFrame::Fatal { .. } => "FATAL",
            ControlFrame::Terminate { force: false } => "TERM",
            ControlFrame::Terminate { force: true } => "KILL",
            ControlFrame::Ack => "ACK",
        }
    }
}

/// Encode a control frame into bytes. The format is:
/// `<u32-le length><UTF-8 line>`.
pub fn encode_frame(frame: &ControlFrame) -> CaduceusResult<Vec<u8>> {
    let line = match frame {
        ControlFrame::Ready { pgid } => {
            format!("v{version} READY {pgid}", version = PROTOCOL_VERSION)
        }
        ControlFrame::Done {
            status,
            signaled: true,
        } => {
            format!(
                "v{version} DONE signal:{status}",
                version = PROTOCOL_VERSION
            )
        }
        ControlFrame::Done { status, .. } => {
            format!("v{version} DONE {status}", version = PROTOCOL_VERSION)
        }
        ControlFrame::Fatal { reason } => {
            format!("v{version} FATAL {reason}", version = PROTOCOL_VERSION)
        }
        ControlFrame::Terminate { force: false } => {
            format!("v{version} TERM", version = PROTOCOL_VERSION)
        }
        ControlFrame::Terminate { force: true } => {
            format!("v{version} KILL", version = PROTOCOL_VERSION)
        }
        ControlFrame::Ack => format!("v{version} ACK", version = PROTOCOL_VERSION),
    };
    if line.len() + 4 > MAX_FRAME_BYTES {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("frame too long: {} bytes", line.len()),
        });
    }
    let mut out = Vec::with_capacity(line.len() + 4);
    out.extend_from_slice(&(line.len() as u32).to_le_bytes());
    out.extend_from_slice(line.as_bytes());
    Ok(out)
}

/// Decode a control frame from a buffer of bytes. Returns the
/// decoded frame plus the number of bytes consumed; the caller
/// passes any leftover bytes back through.
pub fn decode_frame(buf: &[u8]) -> CaduceusResult<(ControlFrame, usize)> {
    if buf.len() < 4 {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: "buffer too short for length prefix".to_string(),
        });
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
        });
    }
    if buf.len() < 4 + len {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: "buffer truncated inside frame".to_string(),
        });
    }
    let line = std::str::from_utf8(&buf[4..4 + len]).map_err(|err| CaduceusError::Worker {
        context: "supervisor:frame",
        stderr: format!("non-UTF-8 frame: {err}"),
    })?;
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let opcode = parts.next().unwrap_or("");
    let payload = parts.next().unwrap_or("");
    if version != format!("v{PROTOCOL_VERSION}") {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("unsupported protocol version {version}"),
        });
    }
    let frame = match opcode {
        "READY" => {
            let pgid: i32 = payload.parse().map_err(|err| CaduceusError::Worker {
                context: "supervisor:frame",
                stderr: format!("invalid READY payload {payload:?}: {err}"),
            })?;
            ControlFrame::Ready { pgid }
        }
        "DONE" => {
            if let Some(rest) = payload.strip_prefix("signal:") {
                let n: i32 = rest.parse().map_err(|err| CaduceusError::Worker {
                    context: "supervisor:frame",
                    stderr: format!("invalid DONE signal payload {payload:?}: {err}"),
                })?;
                ControlFrame::Done {
                    status: n,
                    signaled: true,
                }
            } else {
                let n: i32 = payload.parse().map_err(|err| CaduceusError::Worker {
                    context: "supervisor:frame",
                    stderr: format!("invalid DONE payload {payload:?}: {err}"),
                })?;
                ControlFrame::Done {
                    status: n,
                    signaled: false,
                }
            }
        }
        "FATAL" => ControlFrame::Fatal {
            reason: payload.to_string(),
        },
        "TERM" => ControlFrame::Terminate { force: false },
        "KILL" => ControlFrame::Terminate { force: true },
        "ACK" => ControlFrame::Ack,
        other => {
            return Err(CaduceusError::Worker {
                context: "supervisor:frame",
                stderr: format!("unknown opcode {other:?}"),
            })
        }
    };
    Ok((frame, 4 + len))
}
// ---------------------------------------------------------------------------
// Frame I/O over tokio child streams
// ---------------------------------------------------------------------------

/// Async read a single control frame from `stream`. Returns
/// `None` on EOF (the supervisor closed the pipe).
pub async fn read_frame_async<R>(
    stream: &mut R,
    buf: &mut Vec<u8>,
) -> CaduceusResult<Option<ControlFrame>>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    let mut header = [0u8; 4];
    let n = match stream.read(&mut header).await {
        Ok(0) => return Ok(None),
        Ok(n) => n,
        Err(err) => {
            return Err(CaduceusError::Worker {
                context: "supervisor:frame",
                stderr: format!("read header: {err}"),
            })
        }
    };
    if n < 4 {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("short read on header: {n} bytes"),
        });
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
        });
    }
    buf.clear();
    buf.resize(4 + len, 0);
    buf[..4].copy_from_slice(&header);
    stream
        .read_exact(&mut buf[4..])
        .await
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("read body: {err}"),
        })?;
    let (frame, _) = decode_frame(buf)?;
    Ok(Some(frame))
}

pub async fn write_frame_async<W>(stream: &mut W, frame: &ControlFrame) -> CaduceusResult<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let bytes = encode_frame(frame)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("write: {err}"),
        })?;
    stream.flush().await.ok();
    Ok(())
}
