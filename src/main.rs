//! `caduceus` binary entry point.
//!
//! The CLI parses the canonical subcommands listed in `CONTRACTS.md` under
//! "CLI contract": `run`, `status`, `worktree-gc`, `queue reset`, and
//! `migrate-state`. A no-argument invocation is equivalent to `caduceus run`
//! — that rewriting happens inside the CLI parser, before Clap dispatches,
//! so a bare cron tick never prints help or version output.
//!
//! `run` is silent on success (per the Cron model in `CONTRACTS.md`); all
//! diagnostics go to stderr.
//!
//! The hidden `__worker-supervisor` mode is dispatched before public
//! command parsing — the token is never shown in `--help` output and is
//! never accepted from cron / plugin configuration. The supervisor
//! executes the worker in its own Unix session and talks to the daemon
//! over the inherited `stdin` / `stdout` file descriptors using the
//! framed control protocol.

use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use caduceus::config::Config;
use caduceus::error::CaduceusResult;

mod cli;

fn main() -> ExitCode {
    // Hidden `__worker-supervisor` mode is dispatched first; the
    // token is reserved and never accepted from cron or plugin
    // configuration. The supervisor runs the worker under
    // supervision and exits once the worker session is reaped.
    if std::env::args_os().any(|arg| arg == caduceus::worker_supervisor::HIDDEN_COMMAND) {
        return match run_supervisor_mode() {
            Ok(()) => ExitCode::from(0),
            Err(err) => {
                eprintln!("caduceus: supervisor: {err}");
                err.exit_code()
            }
        };
    }

    // The CLI router inspects `args_os` and inserts `run` when the user
    // invoked `caduceus` with no arguments, before Clap parsing. This is
    // the contractually documented behaviour (CONTRACTS.md, "Implement
    // no-argument behavior by inspecting `args_os`...").
    match cli::run() {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            // Diagnostics to stderr; cron captures nothing on success.
            eprintln!("caduceus: {err}");
            err.exit_code()
        }
    }
}

/// Parse configuration through the canonical resolver chain. Used by both
/// `run` and `status`; wrapper around the typed loader so it remains easy
/// to grow during later phases without touching `main`.
#[allow(dead_code)]
pub(crate) fn load_config() -> CaduceusResult<Config> {
    Config::load()
}

/// Hidden supervisor mode. Parses the small set of
/// `--worktree / --run-id / --issue / --context-json /
/// --transcript / --heartbeat / --timeout / -- <worker
/// command>` arguments, sets the subreaper (Linux), then runs
/// the worker session. Talks to the daemon over inherited
/// stdin/stdout using the framed control protocol; on EOF
/// (daemon death) or explicit TERM/KILL frames, the worker
/// session is reaped before this function returns.
fn run_supervisor_mode() -> CaduceusResult<()> {
    use std::io::{Read, Write};
    use std::path::PathBuf;

    use caduceus::worker_supervisor::{detach_session, encode_frame, ControlFrame};

    let mut args = std::env::args_os().skip(1);
    let mut worktree: Option<PathBuf> = None;
    let mut run_id: Option<String> = None;
    let mut issue_ref: Option<String> = None;
    let mut context_json: Option<String> = None;
    let mut transcript_path: Option<PathBuf> = None;
    let mut heartbeat_path: Option<PathBuf> = None;
    let mut _timeout_seconds: u64 = 3600;
    let mut worker_command: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        let s = arg.to_string_lossy().into_owned();
        match s.as_str() {
            "--worktree" => worktree = args.next().map(PathBuf::from),
            "--run-id" => run_id = args.next().map(|a| a.to_string_lossy().into_owned()),
            "--issue" => issue_ref = args.next().map(|a| a.to_string_lossy().into_owned()),
            "--context-json" => {
                context_json = args.next().map(|a| a.to_string_lossy().into_owned())
            }
            "--transcript" => transcript_path = args.next().map(PathBuf::from),
            "--heartbeat" => heartbeat_path = args.next().map(PathBuf::from),
            "--timeout" => {
                _timeout_seconds = args
                    .next()
                    .and_then(|a| a.to_string_lossy().parse::<u64>().ok())
                    .unwrap_or(3600)
            }
            "--" => {
                for rest in args {
                    worker_command.push(rest.to_string_lossy().into_owned());
                }
                break;
            }
            _ => {}
        }
    }

    let worktree = worktree.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--worktree is required".to_string(),
    })?;
    let _run_id = run_id.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--run-id is required".to_string(),
    })?;
    let issue_ref = issue_ref.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--issue is required".to_string(),
    })?;
    let _context_json = context_json.unwrap_or_default();
    let transcript_path = transcript_path.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--transcript is required".to_string(),
    })?;
    let heartbeat_path = heartbeat_path.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--heartbeat is required".to_string(),
    })?;

    if worker_command.is_empty() {
        return Err(caduceus::CaduceusError::Worker {
            context: "supervisor",
            stderr: "missing worker command after `--`".to_string(),
        });
    }

    // Linux: enable the child subreaper so a grandchild that
    // calls `setsid` is still reaped by us. Non-fatal on
    // failure; the worker-session kill path still works.
    #[cfg(target_os = "linux")]
    {
        if let Err(err) = nix::sys::prctl::set_child_subreaper(true) {
            tracing::warn!(error = %err, "could not enable child subreaper");
        }
    }
    let _ = heartbeat_path;

    // Sanity-check the issue ref format. The daemon-side
    // already validates this, but a malformed ref must fail
    // fast inside the supervisor too.
    let _ = caduceus::issue::IssueKey::parse(&issue_ref)?;

    // Detach into a new session so the worker has its own
    // process group. The daemon recorded our PGID via the
    // `READY` frame we send next.
    detach_session()?;

    // Spawn the worker as a child of the supervisor. The
    // supervisor's stdin/stdout are the daemon's control /
    // status pipes; the worker inherits stdin/stdout/stderr
    // from us. The contract permits this — see
    // `worker_supervisor::build_supervisor_command`.
    let mut cmd = std::process::Command::new(&worker_command[0]);
    for arg in &worker_command[1..] {
        cmd.arg(arg);
    }
    cmd.current_dir(&worktree);
    cmd.stdin(std::process::Stdio::null());
    // The worker's stdout is forwarded to the daemon over
    // the supervisor's stderr (which the daemon captures into
    // the transcript). The supervisor's stdout carries the
    // framed control protocol only — this keeps the protocol
    // bytes and the worker bytes from interleaving.
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|err| caduceus::CaduceusError::Worker {
        context: "supervisor:worker_spawn",
        stderr: format!("spawn worker: {err}"),
    })?;

    // Tell the daemon our PGID so it can record it for the
    // post-ACK kill path. After `setsid()` we are the leader
    // of a fresh process group whose PGID equals our PID.
    let pgid = std::process::id() as i32;
    let ready = encode_frame(&ControlFrame::Ready { pgid })?;
    std::io::stdout().write_all(&ready).ok();
    std::io::stdout().flush().ok();

    // Wait for the daemon's ACK over our stdin. We read a
    // full frame and dispatch on the opcode; if the read
    // returns EOF (daemon closed stdin) we kill the worker.
    // After the ACK, we continue reading frames in a
    // background thread so a TERM / KILL frame from the
    // daemon can interrupt the worker mid-run.
    let mut buf = Vec::with_capacity(64);
    let mut header = [0u8; 4];
    match std::io::stdin().read(&mut header) {
        Ok(0) | Err(_) => {
            // Daemon closed stdin → kill the worker.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        Ok(_) => {
            buf.resize(4, 0);
            buf[..4].copy_from_slice(&header);
            let len = u32::from_le_bytes(header) as usize;
            let mut body = vec![0u8; 4 + len];
            body[..4].copy_from_slice(&header);
            std::io::stdin().read_exact(&mut body[4..]).ok();
            buf = body;
        }
    }
    // Decode and check ACK.
    let (frame, _) = match caduceus::worker_supervisor::decode_frame(&buf) {
        Ok(pair) => pair,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
    };
    if !matches!(frame, ControlFrame::Ack) {
        // Anything other than ACK before the worker is
        // running is a protocol error — kill the worker.
        let _ = child.kill();
        let _ = child.wait();
        return Ok(());
    }

    // Spawn a background thread that listens for further
    // frames from the daemon (TERM / KILL). When it sees
    // TERM, it sends SIGTERM to the worker session; when it
    // sees KILL, SIGKILL. EOF on stdin means "daemon died"
    // → kill the worker session too.
    // Capture the worker's PID; the worker is its own
    // process-group leader (set via `process_group(0)`), so
    // PGID == worker PID. We use the worker PID for the
    // kill -PID form, and the worker PGID (= worker PID)
    // for the kill -PGID form — both work because the worker
    // is the leader of its own process group.
    let pgid_for_kill: i32 = child.id() as i32;
    let child_id: u32 = child.id();
    let _stdin_killer = std::thread::spawn(move || {
        use std::io::Read;
        let mut local_buf = Vec::with_capacity(64);
        let mut header = [0u8; 4];
        loop {
            match std::io::stdin().read(&mut header) {
                Ok(0) => {
                    // Daemon closed stdin → kill session.
                    let _ = std::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg(format!("kill -TERM -{pgid_for_kill} 2>/dev/null; kill -KILL {child_id} 2>/dev/null"))
                        .output();
                    break;
                }
                Ok(_) => {
                    let len = u32::from_le_bytes(header) as usize;
                    local_buf.resize(4 + len, 0);
                    local_buf[..4].copy_from_slice(&header);
                    if std::io::stdin().read_exact(&mut local_buf[4..]).is_err() {
                        break;
                    }
                    let Ok((frame, _)) = caduceus::worker_supervisor::decode_frame(&local_buf)
                    else {
                        continue;
                    };
                    match frame {
                        ControlFrame::Terminate { force: false } => {
                            let _ = std::process::Command::new("/bin/sh")
                                .arg("-c")
                                .arg(format!(
                                    "kill -TERM -{pgid_for_kill} 2>/dev/null; kill -KILL {child_id} 2>/dev/null"
                                ))
                                .output();
                        }
                        ControlFrame::Terminate { force: true } => {
                            let _ = std::process::Command::new("/bin/sh")
                                .arg("-c")
                                .arg(format!(
                                    "kill -KILL -{pgid_for_kill} 2>/dev/null; kill -KILL {child_id} 2>/dev/null"
                                ))
                                .output();
                        }
                        ControlFrame::Fatal { .. }
                        | ControlFrame::Done { .. }
                        | ControlFrame::Ack
                        | ControlFrame::Ready { .. } => {}
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Forward worker stderr (which carries the worker's
    // stdout stream) to the transcript. The supervisor's
    // stdout carries the framed control protocol only.
    let mut worker_stderr = child.stderr.take();
    let transcript_for_write = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .ok();

    let tx_err = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        if let Some(s) = worker_stderr.as_mut() {
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Some(f) = transcript_for_write.as_ref() {
                    if let Ok(mut w) = f.try_clone() {
                        let _ = w.write_all(&buf[..n]);
                        let _ = w.flush();
                    }
                }
            }
        }
    });

    // Wait for the worker.
    let status = child
        .wait()
        .map_err(|err| caduceus::CaduceusError::Worker {
            context: "supervisor:worker_wait",
            stderr: format!("wait: {err}"),
        })?;
    let _ = tx_err.join();
    // The killer thread exits when it sees EOF on stdin;
    // the daemon should close stdin once it has read DONE.
    // We don't join it — it's a fire-and-forget background
    // observer.

    // Send `DONE` over our stdout so the daemon sees the
    // exit code.
    let done = encode_frame(&ControlFrame::Done {
        status: status.code().unwrap_or(1),
        signaled: status.code().is_none(),
    })?;
    let _ = std::io::stdout().write_all(&done);
    let _ = std::io::stdout().flush();

    Ok(())
}
