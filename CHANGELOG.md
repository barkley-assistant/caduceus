# Changelog

All notable changes to Caduceus are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/).

## [1.0.0] - Unreleased

Ongoing development toward the first full release. Not yet tagged or
published. The version bump in the manifest reflects how far the daemon
has come since the initial public release — it is not a ship date.

Caduceus has grown from a single-host worker into a multi-component
daemon with structured crash recovery, bounded concurrency, daemon-owned
repository storage, an OCI executor, and a broad test suite. The CLI
surface, state format, and worker process model are deliberately stable
across this period.

### Added

- **Scheduler leadership.** A single leader is elected per host with
  TTL-based leases. Followers defer to the leader's tick, eliminating
  the dual-tick race that v0.1.0 silently tolerated.
- **Bounded concurrency.** Up to N workers run in flight at once per
  cron tick (bounded-across-the-tick), and a single repository never
  runs two workers concurrently. Replaces the host-wide tick lock.
- **Circuit breakers.** Per-destination breakers trip after consecutive
  failures and cool down on a configured schedule, bounding the blast
  radius of an upstream outage.
- **Daemon-owned repositories.** Repositories live in the daemon's state
  directory, not in the operator's working tree. Workers fork isolated
  worktrees on demand, replacing the v0.1.0 "work in the operator's
  checkout" model.
- **Incremental GitHub discovery.** Discovery scans the known repository
  set first and only falls back to a full list when needed. Per-repo
  ETag caching reduces unauthenticated API calls.
- **Finalization checkpoints.** The reconciler persists a checkpoint
  before each external side effect (push, comment, close, merge-detect)
  and resumes from the last checkpoint on restart. A crash mid-PR no
  longer re-runs the side effect.
- **Reconciler for ambiguous side effects.** When the daemon restarts
  mid-PR, it queries GitHub for the current state and only re-runs the
  side effects that didn't land.
- **Human-review lifecycle.** Pull requests can be placed in a
  human-review-required state that pauses auto-merge and records the
  operator's review decision.
- **OCI executor.** A trait-shaped executor seam with an OCI-backed
  implementation handling mount configuration, secrets, network policy,
  and lifecycle. Replaces the v0.1.0 in-process command runner.
- **Isolation policy enforcement.** The executor enforces a read-only
  root filesystem, an explicit secret allowlist, a deny-by-default
  network policy, and a baseline image.
- **Production configuration bootstrap.** `caduceus setup` generates a
  non-secret configuration with a deterministic token resolution chain.
- **Transactional Hermes scheduling.** The cron-side dispatcher uses a
  hermes-cron CLI subprocess with a JSON-string response contract,
  replacing the unreliable cronjob-MCP path.
- **Worker deadlines.** A configurable timeout kills the worker process
  tree (including orphaned grandchildren) via SIGTERM → SIGKILL with a
  grace window. Replaces the v0.1.0 "wait forever" model.
- **Worker transcripts.** Worker stdout and stderr are persisted to a
  bounded, rotated transcript file and surfaced in status output.
- **Hardened Git invocation.** Every `git` subprocess runs through a
  `GitRunner` that validates arguments, applies a timeout, and captures
  exit codes correctly so signaled kills aren't silently reported as
  success.
- **Integration test suite.** Ten canonical end-to-end scenarios cover
  the four cases v0.1.0 explicitly excluded: happy path, partial PR
  retry, grandchild timeout, and concurrent worker execution.
- **Cross-subsystem failure matrix.** A 70-case matrix pairing each
  subsystem's failure mode with the daemon's observable behaviour.
- **Hermes lifecycle tests.** A real Hermes host fixture drives the
  end-to-end happy path and common operator workflows.
- **Release-binary canary.** The full test suite runs against the
  release artifact, not just the debug build.
- **Operator documentation.** Architecture, configuration, installation,
  plugin lifecycle, CI, public-voice, and Hermes-integration guides
  published under `docs/`.
- **Per-tick claim cap.** `max_issues_per_tick` bounds how many queue
  entries a single tick will claim before returning, so wall-clock per
  tick is predictable. Default `worker_parallelism * 4`; `0` opts into
  the unbounded drain-the-queue behavior. Closes #108.

### Changed

- **Source tree restructured.** The flat `src/` from v0.1.0 is now
  organised by subsystem, with eight oversized modules split by
  responsibility. Planning-era narration and dead references to task
  numbers have been stripped from comments.
- **Doctor output rewritten.** `hermes caduceus doctor` now groups
  findings by severity and surfaces the actual fix, not a category code.
- **Tests reorganised.** Tests live in `tests/<subsystem>/` subfolders
  with a topic-based naming scheme.

### Fixed

- **Dual-writer transcript race.** The daemon no longer opens the run
  transcript; the supervisor is the sole writer of
  `<state-dir>/runs/<run-id>.log` (worker stdout+stderr). Previously
  the daemon re-opened the same path with `truncate(true)` to capture
  supervisor diagnostics, silently clobbering worker bytes written
  before its open. Supervisor stderr now inherits to the daemon's
  stderr. Closes #134. Part of #94.
- **Supervisor stdout capture.** The supervisor now pipes worker stdout
  into the bounded transcript alongside stderr (byte-interleaved, one
  shared writer) instead of discarding it. Previously worker stdout was
  sent to `/dev/null` and only stderr was captured. Closes #126. Part
  of #94.
- **Supervisor cancel/DONE race.** The daemon-side protocol loop drains
  the worker's `DONE` frame before firing the cleanup cancel, so a
  worker that exits 0 cleanly is no longer reported as cancelled. The
  cancel path still wins when fired while the worker is alive and no
  DONE is pending. Closes #130. Part of #94.
- **Supervisor hidden-command dispatch.** The hidden
  `__worker-supervisor` token is now matched only as the first argument
  after the binary name, not anywhere in `argv`, so a copy-pasted
  debugging recipe can no longer accidentally drop a normal CLI
  invocation into supervisor mode. Closes #129. Part of #94.
- **Status exit codes.** Every status call now exits with the documented
  code instead of always returning 0.
- **GitHub auth token resolution.** The daemon now resolves the GitHub
  token through the full chain (config → env → `gh auth token`) and
  degrades gracefully when absent instead of panicking.
- **Cron dispatcher wiring.** The Python plugin calls the hermes-cron
  CLI subprocess and parses the JSON response, replacing the cronjob-MCP
  path that silently lost dispatch on Hermes upgrades.
- **Worker environment injection.** The supervisor injects the required
  `CADUCEUS_*` env vars into the worker subprocess so state and config
  resolve consistently.
- **Worktree lock cleanup.** The daemon removes the empty `.worktrees/.lock`
  file on every exit path — normal, panic, or SIGKILL. The dirty-check
  also tolerates an empty `.worktrees/` directory when no worktree is
  registered. Operators no longer need to manually `rm` the lock file
  between failed dispatches.
- **Investigation finalization checkpoint.** The investigation
  finalization path now persists `InvestigationReady` /
  `InvestigationCommented` checkpoints (queue + SQLite) around the
  findings comment, mirroring the code-ticket durable-checkpoint
  pattern. A crash after the comment no longer re-dispatches the
  worker and posts a duplicate comment. Closes #120.
- **Code-ticket pre-PR queue checkpoints.** The code finalization
  path now persists a durable queue `FinalizationCheckpoint` at
  `ResultValidated` / `Committed` / `Pushed` as well as `PrCreated`
  (SQLite first, then queue), mirroring the #120 pattern. A crash
  after a commit or push resumes at the recorded stage instead of
  re-dispatching the worker and creating a duplicate branch/commit.
  Closes #119.
- **Supervisor ACK gate.** The supervisor spawns the worker only after
  the daemon ACKs the `READY(pgid)` frame, so no worker process exists
  before the PGID is confirmed. Closes #125. Part of #94.
- **Supervisor frame length validation and partial-header reads.** The
  supervisor's stdin readers now validate the frame length against
  `MAX_FRAME_BYTES` before allocating, and use `read_exact` for the
  4-byte header so a partial read can no longer be parsed as a length
  prefix. A malformed `0xFFFFFFFF` header or a truncated header from a
  crashed/hostile worker is rejected without a multi-GB allocation.
  Closes #127. Closes #128. Part of #94.

### Security

- **Adversarial isolation tests.** Escape, leak, network, and
  cancellation tests verify the executor cannot reach the host
  filesystem, leak secrets into the worker environment, or escape the
  network policy.
- **Network policy.** Outbound traffic from the executor is
  allowlist-only by default.
- **Mount and secret allowlist.** The executor refuses to mount
  non-allowlisted paths or load secrets outside the explicit allowlist.

### Known Limitations

- **Pre-existing test fragility.** A handful of integration tests fail
  in environments where the `caduceus` binary isn't on the default PATH
  or where `caduceus doctor` returns a non-zero exit code. These
  reproduce on v0.1.0 and are tracked as environment-dependence, not
  regressions.
- **Linux-only.** The worker supervisor's process-group semantics
  (`prctl`, `/proc`, `setsid`) are not yet portable to macOS. A
  follow-up ticket will address this.
- **No public release artifacts.** The v1.0.0 label is an internal
  version bump; no GitHub release is cut from this version.

## [0.1.0] - 2026-07-15

Initial public release. Supports a single host and worker, PAT
authentication, JSON state, dry runs, investigation tickets, and Hermes Agent.

### Added

- Rust daemon, Python reference bridge, and Hermes plugin.
- Dry-run and investigation-ticket workflows.
- Migration with `caduceus migrate-state` and corruption
  recovery with `caduceus recover-state`.
- Worker supervision, claim and heartbeat handling, and a nonblocking
  whole-tick lock.
- An operator migration guide in [MIGRATION.md](MIGRATION.md).

### Known Limitations

- One issue is processed per host-wide tick; a slow worker blocks
  later work.
- Authentication uses a personal access token only.
- State is JSON, written atomically with `temp + fsync + rename`.
- `caduceus status` exit codes were not mapped to the CLI contract;
  every status call exited 0 regardless of outcome.
- The release did not include runtime tests for code and investigation
  success, partial PR-response retry, timeout with a grandchild, or
  concurrent worker execution.

### Security

- Did not publish a security contact or disclosure policy.

[next]: https://github.com/barkley-assistant/caduceus/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/barkley-assistant/caduceus/releases/tag/v0.1.0
