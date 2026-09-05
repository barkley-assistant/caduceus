//! `caduceus` binary entry point.
//!
//! The CLI parses the canonical subcommands listed in the CLI contract in
//! `src/cli/mod.rs`: `run`, `status`, `worktree-gc`, `queue reset`, and
//! `migrate-state`. A no-argument invocation is equivalent to `caduceus run`
//! — that rewriting happens inside the CLI parser, before Clap dispatches,
//! so a bare cron tick never prints help or version output.
//!
//! `run` is silent on success; all diagnostics go to stderr.
//!
//! The hidden `__worker-supervisor` mode is dispatched before public
//! command parsing — the token is matched only as the first argument
//! after the binary name (`argv[1]`), is never shown in `--help`
//! output, and is never accepted from cron / plugin configuration.
//! The supervisor executes the worker in its own Unix session and
//! talks to the daemon over the inherited `stdin` / `stdout` file
//! descriptors using the framed control protocol.

use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use caduceus::error::CaduceusResult;

mod cli;

fn main() -> ExitCode {
    // Hidden `__worker-supervisor` mode is dispatched first; the
    // token is reserved and never accepted from cron or plugin
    // configuration. The token is matched only as the first argument
    // after the binary name (`argv[1]`), exactly how
    // `build_supervisor_command` constructs the child argv. The
    // supervisor runs the worker under supervision and exits once
    // the worker session is reaped.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == caduceus::worker_supervisor::HIDDEN_COMMAND)
    {
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
    // the contractually documented behaviour (the CLI contract in
    // `src/cli/mod.rs`: "Implement no-argument behavior by inspecting
    // `args_os`...").
    //
    // Block SIGINT/SIGTERM before any CLI work when this process is a
    // daemon tick (`caduceus` / `caduceus run`), so a signal delivered
    // during startup pends instead of hitting the default disposition
    // and killing the process (issue #270). `run_blocking` installs
    // the tokio handlers and restores the mask after registration.
    // Other subcommands (`status`, `queue reset`, `doctor`, ...) keep
    // their default signal behaviour, and the supervisor mode above is
    // exempted so the worker TERM-to-KILL contract is unaffected.
    let is_tick_invocation = std::env::args_os().nth(1).is_none_or(|arg| arg == "run");
    if is_tick_invocation {
        if let Err(err) = caduceus::signals::block_idle_signals() {
            eprintln!("caduceus: {err}");
            return ExitCode::from(1);
        }
    }

    match cli::run() {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            // Diagnostics to stderr; cron captures nothing on success.
            eprintln!("caduceus: {err}");
            err.exit_code()
        }
    }
}

/// Hidden supervisor mode. Parses the small set of
/// `--worktree / --run-id / --issue / --context-json /
/// --transcript / --heartbeat / --timeout / -- <worker
/// command>` arguments, sets the subreaper (Linux), then runs
/// the worker session. Talks to the daemon over inherited
/// The supervisor runs as a hidden subcommand of the caduceus binary
/// (see process_lifecycle::HIDDEN_COMMAND). It exchanges control
/// frames with the daemon over the supervisor's stdin/stdout using
/// the framed control protocol. Flow:
///
/// 1. `detach_session()` makes the supervisor a session leader
/// 2. send `READY{pgid}` to the daemon and flush stdout
/// 3. block on the daemon's `ACK` (30s handshake timeout)
///    - EOF / decode error / unexpected frame → emit FATAL, exit clean
///    - `Terminate` frame → clean cancel, no worker spawned
///    - timeout → emit FATAL, exit 124
/// 4. on `ACK`: build `Command`, set process_group(0), spawn worker
/// 5. start stdin-killer + stdout/stderr-drain threads, wait, write `DONE`
///
/// Until step 4 the daemon has not confirmed the PGID and no child
/// process exists. On EOF, KILL, or TERM after spawn, the worker
/// session is reaped before this function returns.
fn run_supervisor_mode() -> CaduceusResult<()> {
    use std::io::{Read, Write};
    use std::path::PathBuf;

    use caduceus::worker_supervisor::{
        detach_session, encode_frame, kill_pgid, kill_pid, signal_number_from_str,
        BoundedTranscriptWriter, ControlFrame, TREE,
    };

    let mut args = std::env::args_os().skip(1);
    let mut worktree: Option<PathBuf> = None;
    let mut run_id: Option<String> = None;
    let mut issue_ref: Option<String> = None;
    let mut context_json: Option<String> = None;
    let mut transcript_path: Option<PathBuf> = None;
    let mut heartbeat_path: Option<PathBuf> = None;
    let mut _timeout_seconds: u64 = 3600;
    let mut transcript_max_bytes: u64 = 10 * 1024 * 1024;
    let mut worker_command: Vec<String> = Vec::new();
    let mut issue_title: Option<String> = None;
    let mut issue_body: Option<String> = None;
    let mut issue_labels_json: Option<String> = None;
    let mut branch_name: Option<String> = None;

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
            "--transcript-max-bytes" => {
                transcript_max_bytes = args
                    .next()
                    .and_then(|a| a.to_string_lossy().parse::<u64>().ok())
                    .unwrap_or(10 * 1024 * 1024)
            }
            "--issue-title" => issue_title = args.next().map(|a| a.to_string_lossy().into_owned()),
            "--issue-body" => issue_body = args.next().map(|a| a.to_string_lossy().into_owned()),
            "--issue-labels-json" => {
                issue_labels_json = args.next().map(|a| a.to_string_lossy().into_owned())
            }
            "--branch-name" => branch_name = args.next().map(|a| a.to_string_lossy().into_owned()),
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
    let run_id = run_id.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--run-id is required".to_string(),
    })?;
    let issue_ref = issue_ref.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--issue is required".to_string(),
    })?;
    let context_json = context_json.unwrap_or_default();
    let transcript_path = transcript_path.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--transcript is required".to_string(),
    })?;
    let heartbeat_path = heartbeat_path.ok_or_else(|| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "--heartbeat is required".to_string(),
    })?;

    let issue_title = issue_title.unwrap_or_default();
    let issue_body = issue_body.unwrap_or_default();
    let issue_labels_json = issue_labels_json.unwrap_or_else(|| "[]".to_string());
    let branch_name = branch_name.unwrap_or_default();

    if worker_command.is_empty() {
        return Err(caduceus::CaduceusError::Worker {
            context: "supervisor",
            stderr: "missing worker command after `--`".to_string(),
        });
    }

    // Enable the child subreaper where the platform supports it. Non-fatal
    // on failure; the worker-session kill path still works.
    if let Err(err) = TREE.adopt_subtree(std::process::id() as i32) {
        tracing::warn!(error = %err, "could not enable child subreaper");
    }
    let _ = heartbeat_path;

    // Sanity-check the issue ref format. The daemon-side
    // already validates this, but a malformed ref must fail
    // fast inside the supervisor too.
    let issue = caduceus::issue::IssueKey::parse(&issue_ref)?;

    // Detach into a new session so the worker has its own
    // process group. The daemon records our PGID via the
    // `READY` frame we send next.
    detach_session()?;

    // The supervisor process flow:
    //
    // 1. parse args + set subreaper
    // 2. detach_session() — make the supervisor a session leader
    // 3. send `READY{pgid}` to the daemon via stdout
    // 4. block on stdin for the daemon's `ACK` (with 30s timeout)
    // 5. on valid `ACK`: build `Command`, set process_group(0), spawn the worker
    // 6. start stdin-killer + stderr-drain threads
    // 7. wait for the worker, then write `DONE`
    //
    // Until step 4 (ACK received) no child process exists. If the daemon
    // hangs (no ACK within 30s) we exit with code 124 and a FATAL frame.
    // If the daemon dies (EOF before ACK) we exit cleanly with FATAL.

    // Tell the daemon our PGID so it can record it for the
    // post-ACK kill path. After `setsid()` we are the leader
    // of a fresh process group whose PGID equals our PID.
    let pgid = std::process::id() as i32;
    let ready = encode_frame(&ControlFrame::Ready { pgid })?;
    std::io::stdout().write_all(&ready).ok();
    std::io::stdout().flush().ok();

    // Wait for the daemon's ACK over our stdin, with a 30s
    // handshake timeout. Until the ACK arrives no worker
    // process exists: EOF (daemon died), a decode error, or a
    // non-ACK frame all abort the handshake without spawning,
    // while `Terminate` is a clean cancellation.
    enum PreAck {
        Ack,
        Terminate,
        Fatal(String),
        Timeout,
    }
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    let _ack_reader = std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(caduceus::worker_supervisor::MAX_FRAME_BYTES);
        // `read_frame_sync` reads the header with `read_exact` and
        // validates `len <= MAX_FRAME_BYTES` before allocating, so a
        // partial header or an oversize length is rejected without a
        // multi-GB allocation — defects #127/#128 in #94.
        let outcome =
            match caduceus::worker_supervisor::read_frame_sync(&mut std::io::stdin(), &mut buf) {
                Ok(Some(ControlFrame::Ack)) => PreAck::Ack,
                Ok(Some(ControlFrame::Terminate { .. })) => PreAck::Terminate,
                Ok(Some(other)) => PreAck::Fatal(format!("unexpected frame before ACK: {other:?}")),
                Ok(None) => PreAck::Fatal("daemon EOF before ACK".to_string()),
                Err(err) if err.kind() == std::io::ErrorKind::InvalidData => PreAck::Fatal(
                    format!("daemon sent frame length exceeding cap before ACK: {err}"),
                ),
                Err(err) => PreAck::Fatal(format!("daemon error before ACK: {err}")),
            };
        let _ = ack_tx.send(outcome);
    });
    let pre_ack = match ack_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(pre_ack) => pre_ack,
        Err(_) => PreAck::Timeout,
    };
    match pre_ack {
        PreAck::Ack => {}
        PreAck::Terminate => return Ok(()),
        PreAck::Timeout => {
            let fatal = encode_frame(&ControlFrame::Fatal {
                reason: "handshake timeout".to_string(),
            })?;
            std::io::stdout().write_all(&fatal).ok();
            std::io::stdout().flush().ok();
            // `_ack_reader` is still blocked on `stdin().read_exact`.
            // That's intentional — `exit(124)` reaps the thread via
            // process exit. A future maintainer must not "fix" this
            // to `return Ok(())` without also draining the reader.
            std::process::exit(124);
        }
        PreAck::Fatal(reason) => {
            let fatal = encode_frame(&ControlFrame::Fatal { reason })?;
            std::io::stdout().write_all(&fatal).ok();
            std::io::stdout().flush().ok();
            return Ok(());
        }
    }

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
    // The worker's stdout and stderr are piped and drained into
    // the bounded transcript (see the drain block below). The
    // supervisor's own stdout carries the framed control protocol
    // only — this keeps the protocol bytes and the worker bytes
    // from interleaving.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // When the daemon supplied the new issue-context flags,
    // build a sanitized worker environment from them and
    // apply it before the worker starts. Backward-compatible
    // callers (e.g. existing tests that drive
    // `__worker-supervisor` by hand) omit these flags; we
    // leave the inherited environment untouched in that case.
    if !branch_name.is_empty() {
        let labels: Vec<String> = if issue_labels_json.trim() == "[]" {
            Vec::new()
        } else {
            serde_json::from_str(&issue_labels_json).map_err(|err| {
                caduceus::CaduceusError::Config(format!("invalid labels JSON: {err}"))
            })?
        };
        let inputs = caduceus::worker::SanitizedEnvInputs {
            target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
                key: issue,
                title: issue_title,
                body: issue_body,
                labels,
                branch_name,
            }),
            worktree_path: worktree.clone(),
            run_id: run_id.clone(),
            allowlist: Vec::new(),
            context_json,
        };
        let parent: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        let env = caduceus::worker::sanitized_env(&parent, &inputs)?;
        cmd.env_clear();
        cmd.envs(env);
    }

    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|err| caduceus::CaduceusError::Worker {
        context: "supervisor:worker_spawn",
        stderr: format!("spawn worker: {err}"),
    })?;

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
        let mut local_buf = Vec::with_capacity(caduceus::worker_supervisor::MAX_FRAME_BYTES);
        // Send `signal` to the process group; fall through to KILL the
        // PID directly in case the process group is empty (worker has
        // already exec'd or the group is otherwise unreachable).
        let send_signal = |signal: &str| {
            let Some(signal_number) = signal_number_from_str(signal) else {
                return;
            };
            kill_pgid(pgid_for_kill, signal_number);
            kill_pid(child_id as i32, signal_number);
        };
        loop {
            // `read_frame_sync` validates the length before allocating
            // and uses `read_exact` for the header, so neither an
            // oversize nor a partial header can OOM or corrupt the
            // parse — defects #127/#128 in #94.
            match caduceus::worker_supervisor::read_frame_sync(
                &mut std::io::stdin(),
                &mut local_buf,
            ) {
                Ok(None) => {
                    // Daemon closed stdin → kill session.
                    send_signal("TERM");
                    break;
                }
                Ok(Some(ControlFrame::Terminate { force: false })) => {
                    send_signal("TERM");
                }
                Ok(Some(ControlFrame::Terminate { force: true })) => {
                    send_signal("KILL");
                }
                Ok(Some(_)) => {
                    // Not a signal frame — ignore and keep listening.
                }
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Daemon closed stdin mid-frame → the session is
                    // orphaned → kill it.
                    send_signal("TERM");
                    break;
                }
                Err(_) => {
                    // Malformed frame (oversize length, bad opcode):
                    // the daemon is still connected. Ignore it and
                    // keep listening so one bad frame cannot silence
                    // the killer.
                }
            }
        }
    });

    // Forward worker stdout and stderr to the bounded transcript
    // writer, byte-interleaved into the one shared file. Two drain
    // threads share the writer through an `Arc<Mutex<>>`, the same
    // pattern the daemon-side transcript capture uses in
    // `dispatch.rs`. Both streams compete for the single
    // `transcript_max_bytes` budget; a flood drops bytes behind the
    // truncation marker, identical to the old stderr-only drain.
    let worker_stdout = child.stdout.take();
    let worker_stderr = child.stderr.take();
    let writer = std::sync::Arc::new(std::sync::Mutex::new(
        BoundedTranscriptWriter::new(transcript_path.clone(), transcript_max_bytes).map_err(
            |err| {
                tracing::warn!(error = %err, "failed to open transcript");
                err
            },
        )?,
    ));

    let tx_out = {
        let writer = std::sync::Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            if let Some(mut s) = worker_stdout {
                loop {
                    match s.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => writer.lock().unwrap().write_bytes(&buf[..n]),
                        Err(_) => break,
                    }
                }
            }
        })
    };
    let tx_err = {
        let writer = std::sync::Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            if let Some(mut s) = worker_stderr {
                loop {
                    match s.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => writer.lock().unwrap().write_bytes(&buf[..n]),
                        Err(_) => break,
                    }
                }
            }
        })
    };

    // Wait for the worker.
    let status = child
        .wait()
        .map_err(|err| caduceus::CaduceusError::Worker {
            context: "supervisor:worker_wait",
            stderr: format!("wait: {err}"),
        })?;
    // Join both drain threads, then recover the writer. The threads
    // drop their `Arc` clones on exit, so `try_unwrap` succeeds.
    tx_out.join().map_err(|_| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "stdout drain thread panicked".to_string(),
    })?;
    tx_err.join().map_err(|_| caduceus::CaduceusError::Worker {
        context: "supervisor",
        stderr: "stderr drain thread panicked".to_string(),
    })?;
    let writer = std::sync::Arc::try_unwrap(writer)
        .map_err(|_| caduceus::CaduceusError::Worker {
            context: "supervisor",
            stderr: "transcript writer still referenced".to_string(),
        })?
        .into_inner()
        .map_err(|_| caduceus::CaduceusError::Worker {
            context: "supervisor",
            stderr: "transcript writer lock poisoned".to_string(),
        })?;

    // Finalize transcript — report truncation/write failures.
    // This must happen before DONE so a failed write is reported as a
    // worker failure rather than a successful run.
    writer.finalize()?;

    // Send `DONE` over our stdout so the daemon sees the exit code.
    let done = encode_frame(&ControlFrame::Done {
        status: status.code().unwrap_or(1),
        signaled: status.code().is_none(),
    })?;
    std::io::stdout().write_all(&done).ok();
    std::io::stdout().flush().ok();
    Ok(())
}
