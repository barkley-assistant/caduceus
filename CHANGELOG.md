# Changelog

All notable changes to Caduceus are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/).

## [Unreleased]

### Security

- **Fork trust posture documented.** Fork review executes
  attacker-controlled content (fork head SHA, diff, repo metadata);
  the containment, credential-path analysis, and the operator opt-in
  contract (including the private-fork case) are documented in
  `docs/security/fork-trust-posture.md`, with DAR §11.2 updated and a
  new §11.4 pointer. Corpus fixtures `15-17` certify the fork
  injection vectors through the existing adversarial-escape harness.
  Closes #337.

### Changed

- **Configurable git author identity.** `git_author_name` and
  `git_author_email` now resolve per field through explicit config, host git
  config, and the `Caduceus Daemon <caduceus@daemon.local>` fallback. Closes
  #210.

### Breaking

- **Host networking removed from the OCI sandbox.** The `unrestricted`
  value of `sandbox.network` (and the `--network host` argv it rendered)
  no longer exists: host networking is structurally unrepresentable, and a
  config carrying `network: unrestricted` fails at load with a typed
  unknown-variant error. Migration: set `network: none` (the only value).
- **`sandbox.reserved_host_disk_mb: 0` now disables the host disk-pressure
  watchdog** (previously rejected as invalid). Any positive value is the
  free-space floor in MB (default `2048`); `0` means no sampling and no
  enforcement. Closes #245.

### Added

- **Fork trust policy + quarantine fetch.** `auto_review.fork_policy.allow_fork_prs`
  (per-repo opt-in list, default empty → fail-closed) lets opted-in
  repos review fork PRs through a per-PR quarantine clone: cloned from
  the trusted base URL, SHA-anchored fetch from the fork URL, merge
  base computed inside, removed at terminal status or by a per-tick
  orphan sweep with a forensic `.removed` marker. The Phase-1
  single-origin mirror path for non-fork PRs is unchanged, and denied
  forks keep the Phase-1 `review_skipped_fork_unsupported` event
  byte-for-byte. Closes #337.
- **Review observability CLI.** `caduceus review status
  [<owner/repo>] | list | show <owner/repo> <pr> [--json]` reads the
  review stores (queue, per-`(repo, pr)` state, history) on BOTH state
  backends with a versioned `review/1.0` JSON envelope and the full DAR
  §13 per-row field set; `show` parses history `result_json` documents
  defensively (older schema versions surface raw with a `parse_error`,
  never back-migrated). `execution status` is derived from the latest
  same-generation history row's `ReviewResult.status` and is
  deliberately distinct from `verdict`. Closes #318.
- **DAR §13 event catalog assertion.** `review_event_catalog_test`
  pins every structured review event name to its §13 literal at one
  seam (`caduceus::runtime::audit::review_events`), keeping
  `review_failed_verdict` and `review_execution_failed` textually and
  structurally distinct (AC3). Closes #318.
- **Review mutation-violation enforcement.** A review worker that
  modifies tracked files or daemon control files (`worker-prompt.md`,
  `review-worktree.json`) now fails terminally: the review entry routes
  to `NeedsAttention` with a recovery hint pointing at the preserved
  worktree, the retry budget is not burned, and a
  `review_mutation_violation` event is emitted. Closes #306.
- **ReviewResult verdict-consistency validator and review result
  ingress.** The review worker's result document is now fully
  validated: `review` present iff `status` is `success`; `verdict`
  must be `fail` iff at least one finding is `blocking`; finding
  paths must be repo-relative (no leading `/`, no `..`, no control
  characters) and 1-based lines require a path. Every rejection —
  including unknown `schema_version` values — is an execution
  failure classified `FailureClass::Worker` and burns normal retry
  budget (DAR §8). Closes #305.
- **Review worker prompt and OCI review sandbox profile.** The review
  worker prompt is sectioned and trust-separated in a fixed order
  (daemon policy and output schema trusted first; PR metadata,
  merge-base diff, repository context, and PR discussion untrusted and
  fence-escaped), with deterministic per-section byte budgets measured
  on fence-escaped text — a 1 MiB diff skip threshold, 64 KiB metadata,
  256 KiB repository context, 64 KiB discussion (tail-sampled), and a
  2 MiB total backstop. A diff over budget skips the run via the
  structured `review_skipped_oversized_pr` event, never consuming the
  normal worker retry budget. The OCI sandbox host-path allow-list is
  now target-aware: PR-review worktrees are admitted only under
  `<repo_storage_root>/worktrees/review`, and a review profile resolver
  (`resolve_review_sandbox`) fails closed on non-PR targets while
  asserting the DAR §6.4 posture. The dispatch wiring itself is #339.
  Closes #303.
- **CLI reference page.** `docs/cli.md` documents every `caduceus`
  subcommand and flag, exit codes, JSON envelope versions, locking and
  refusal semantics, and the `hermes caduceus` wrapper surface; the
  README CLI section and the Operator's Manual index link to it. The
  plugin skill now covers all four queue actions including `queue
  reprocess`. Closes #264.
- **Auto Review documentation.** User guide, config reference,
  migration guide, operator recovery cookbook, and release notes for
  Auto Review (PR-revision code review). The canonical engineering
  spec is `docs/architecture/auto-review.md`; the new operator page is
  `docs/auto-review.md`. Closes #325.
- **Queue inspection and removal CLI.** `caduceus queue show
  [<owner/repo#n>] [--json]` lists every entry as a human table (or
  full detail including the finalization checkpoint) with a versioned
  `queue/1.0` JSON envelope; `caduceus queue remove <owner/repo#n>
  [--dry-run] [--force] [--json]` drops a queue entry entirely —
  refusing `InProgress` / `AwaitingReview` / `Done` by default and an
  active claim file always, never touching the worktree, claim file,
  remote branch, or PR. The `hermes caduceus` wrapper now passes
  `queue`, `worktree-gc`, and `migrate-state` through to the binary
  and forwards `--json` on `status`. Closes #265.
- **Hardened per-run OCI baseline** (non-weakenable, both engines):
  `--memory-swap` pinned equal to `--memory` (no swap doubling), bounded
  engine logs (`--log-opt max-size=10m max-file=3`), explicit resolve-time
  denial of devices, engine/runtime socket mounts, and host namespace
  sharing, bounded daemon-side diagnostic log capture (1 MiB cap) under
  `<state_dir>/oci-runs/<run_id>/engine.log`, and resource floors
  `tmpfs_mb >= 1` / `shm_mb >= 1` so a zero tmpfs size cannot silently
  apply an engine-default unbounded size.
- **Host disk-pressure watchdog.** `sandbox.reserved_host_disk_mb` is now
  consumed as a free-space floor sampled every 30 s across the
  device-ID-deduped filesystems hosting the state dir, repo storage, and
  OCI output dirs. On breach, in-flight OCI work is terminated via the
  existing stop path and new OCI dispatch is refused with a typed
  `OciDiskPressure` error until the reserve recovers (256 MiB hysteresis).
  This is a host-level mitigation — `/workspace` remains a host bind mount
  with no per-container byte quota.
- **Auto Review domain types.** `caduceus::review` now defines the
  Phase-1 review domain model — `ReviewTarget` (immutable
  `(repository, PR, head SHA)` identity with persisted merge-base
  context), `ReviewState` (per-PR current pointer with the monotonic
  `review_generation` publication guard), and the
  `ReviewResult`/`Review`/`Finding` worker contract with strict serde,
  snake_case enums, and parse-time field caps. Foundation for the Auto
  Review epic (#290); no behaviour change yet. Closes #292.
- **Review worktree mode.** Review worktrees are materialised as
  disposable, non-pushable detached-HEAD checkouts at the exact PR
  head SHA against the daemon-owned bare mirror — no issue-dispatch
  branch artefact — carrying a versioned `review-worktree.json`
  metadata sidecar with the frozen `base_sha` / `base_ref` /
  `merge_base` for merge-base (three-dot) diffing. A review reaper
  reclaims stale entries via `worktree-gc` / the tick. Closes #299.
- **Phase-1 fork gate for PR discovery.** Fork PRs — and PRs whose head
  repository cannot be identified, such as deleted head branches — are
  skipped before admission with a structured
  `review_skipped_fork_unsupported` event. The gate is unconditional:
  there is no configuration to enable fork review in Phase 1. Closes
  #316.
- **Auto Review configuration block.** New `auto_review:` config section
  (`enabled`, `draft_pull_requests`, both default `false`) opts a daemon
  into automatic PR review; enabling it requires `executor_mode: oci`
  with a valid `sandbox:` section (TrustedHost + `enabled: true` now
  fails config load with an actionable error). New top-level
  `max_reviews_per_tick` (default `worker_parallelism * 4`, `0` =
  unbounded) bounds per-tick review admission. `ticket_label_investigation`
  is deprecated: explicit config use now emits a warning; the key is
  removed in a future release. Closes #320.
- **Target-neutral worker boundary.** The executor/supervisor boundary
  now carries a `WorkTarget` — `Issue(IssueWorkTarget)` keeps the
  historical issue payload (byte-for-byte env and supervisor argv
  compatibility) while `PullRequest(ReviewTarget)` carries the frozen
  review identity with no synthetic issue key and no branch name.
  Review runs export `CADUCEUS_WORK_TARGET=pr` plus
  `CADUCEUS_PR_NUMBER` / `CADUCEUS_PR_REPO` / `CADUCEUS_PR_BASE_SHA` /
  `CADUCEUS_PR_HEAD_SHA`; `CANONICAL_WORKER_ENV_VARS` is now the union
  of the issue and PR path sets. The worker bridge is mode-aware: PR
  runs resolve only the mode-correct result path and never synthesise
  a legacy result (missing result file = execution failure). Closes
  #346.
- **Review persistence stores with atomic v8 activation.** Three
  sibling review-era stores land under BOTH state backends: the
  sibling review queue (entries keyed by the canonical
  `owner/repo#pr@head_sha` form of a persisted `ReviewTarget`
  including `merge_base`, with the issue queue's claim lifecycle
  mirrored into a separate `review-claims/` directory), the
  per-`(repo, pr)` `ReviewState` store (all DAR §3 fields including
  the monotonic `review_generation`, upserted with a never-regress
  CAS guard), and the append-only `review_history` (identity =
  `review_run_id`, unique per completed run; the
  `(repo, pr, head_sha)` tuple is deliberately NOT unique and the
  verbatim version-tagged `ReviewResult` JSON blob is the canonical
  durable result). The store envelope activates atomically:
  `SCHEMA_VERSION` 7→8 (new `review_queue_entries` / `review_state` /
  `review_history` tables + indexes, structural `v7→v8` migration
  step) and the JSON `state.json` envelope 1→2 with an in-place
  accept-and-upgrade of v1 files — a pre-#295 binary hard-fails on
  review-era state on both backends. Same-SHA dedup is a read path
  (`last_reviewed_head_sha` + active-queue check), never a history
  uniqueness constraint. Known follow-up (owned by the Auto Review
  epic): `migrate-state --to-sqlite` does not carry the JSON review
  sidecar files across a backend switch. Closes #295.
- **Review claim-uniqueness and crash-recovery verification.** New
  integration tests prove the review pipeline's concurrency and
  crash-recovery guarantees: single claim per `ReviewTarget` under
  concurrent admission; container-kill recovery with no orphan review
  claims (issue #371); restart-mid-publish with no duplicate comment;
  out-of-order completion with stale-generation suppression; and
  terminal paths (mutation violation / head-SHA gone / PR gone /
  oversized diff) that survive restart without burning the worker
  retry budget. Closes #314.

### Fixed

- **`review list` and `review status` now show each entry's own
  verdict.** The verdict column is derived from the entry's latest
  same-generation history row's `ReviewResult` document (mirroring the
  derived execution status), so superseded SHA entries no longer
  display the PR's current verdict — historical FAIL reviews render as
  `fail`. Entries with no completed run fall back to the PR's last
  verdict; unreadable (old-schema or malformed) result documents
  surface as `-`/null per the defensive-parse contract. Closes #387.
- **E2E fixtures no longer orphan `git daemon` processes on interrupted
  runs.** `GitDaemon::start` now holds an exclusive `flock` on
  `<fixture-root>/git-daemon.lock` for the daemon's lifetime and writes
  `<fixture-root>/git-daemon.pid` right after spawn. Every start first
  reaps leftovers from previous runs that died hard (SIGKILL, abort,
  crash): for ownerless stale roots (lock free) whose pidfile points at
  a live `git daemon` serving exactly that root's base-path, the daemon
  is killed and the root removed. Identity is checked via
  `/proc/<pid>/cmdline` (Linux) or `ps -ww` (macOS), so an unrelated
  process is never signalled and a live test's root is never touched.
  Closes #383.
- **`caduceus run` now initialises file logging.** The CLI `run`
  handler — the entry path the cron pulse wrapper and bare `caduceus`
  invocations actually take — never called `logging::init`, so
  `<state_dir>/processor.log` was never created: the structured log
  stream, and even the handler's own git-identity warning, were
  silently dropped because no tracing subscriber was installed. The
  handler now initialises logging right after config resolution —
  mirroring the documented `tick::run` order — and flushes the
  non-blocking writer before `std::process::exit`. Closes #386.
- **Finalize-path GETs no longer fail on a benign 304.** The ETag-cached
  GitHub client answers an unchanged list/state GET with HTTP 304 plus
  the cached body; eight status checks (PR-list and comment-list reads
  across the finalize path, crash-resume reconciliation, and the
  trigger verifier) treated any non-200 as a failure, so a finalize
  retry or crash-resume on an unchanged URL failed permanently. All
  eight now parse the cached body exactly like a 200. Closes #396.
- **PR merge-status poll no longer fails on a benign 304.** The
  GitHub client's ETag cache legitimately answers an unchanged PR
  with HTTP 304 plus the cached body; the merge-status poll treated
  any non-200 as a failure, so the Auto Review finalizer classified
  the poll as a publish failure and retried with exponential backoff
  forever — the sticky PR comment never appeared. A 304 now parses
  the cached body exactly like a 200, so merged and closed states
  survive unchanged. Closes #385.
- **Successful ticks now project the real next-allowed-poll time.**
  The tick's success path recorded `next_allowed_poll_at` with a
  hard-coded poll interval of 0, so after every successful tick
  `caduceus status` showed a "next allowed poll" in the past — the
  tick's own start time. The configured `poll_interval_seconds` is
  now threaded through, so the field shows when the cadence gate
  actually opens next. Display-only: the gate itself reads
  `last_tick_finished`, never `next_allowed_poll_at`.
- **Cadence-skipped ticks no longer slide the poll gate.** A tick
  skipped by the cadence gate recorded its own timestamp as
  `last_tick_finished`, re-arming the window from the skip: with the
  default `poll_interval_seconds: 120` and a 2-minute cron, every
  invocation landed inside the freshly-slid window and skipped again —
  a silent, permanent poll loop. A `SkippedCadence` outcome now
  records only its outcome for `caduceus status`; `last_tick_finished`
  and `next_allowed_poll_at` keep the last completed tick's values, so
  the gate measures from real work. Closes #384.
- **Live OCI certification suite: image precedence and exit-code
  readback (issue #252).** The shared live-test fixture now applies
  `CADUCEUS_LIVE_TEST_IMAGE` (the reference image) to every test;
  only `image_neutrality_custom_unrelated_image_live` reads
  `CADUCEUS_LIVE_NEUTRALITY_IMAGE` and scopes it to its own sandbox,
  so CI legs no longer run the bulk of the adversarial suite against
  alpine. `run_container` parses the first space-separated field of
  `docker inspect` (not the whole three-field string), restoring the
  authoritative exit-code readback for OOM/cancellation/timeout
  assertions. The `oci-live-certification` job also runs under a new
  `ci` nextest profile with a JUnit emitter, so the failure-diagnostics
  artifact is real.
- **Status commands in non-Hermes shells.** `hermes caduceus status` (and
  every adapter subcommand shelling out to the daemon binary) no longer
  fails with `no configuration source found` in shells that do not export
  `HERMES_HOME`. The adapter now injects its computed default into the
  child environment only when the variable is unset; explicit overrides
  (multi-profile hosts) are preserved and a deliberately-empty value
  still reaches the binary's guard. Closes #263.
- **Completion-gated discovery dedup for Auto Review.** The daemon now
  persists `ReviewState.last_reviewed_head_sha` when a review COMPLETES
  (in `run_review_claim`, before the terminal `Done` transition), not
  only at publication finalization. Previously the tick's 5.6 finalizer
  ran before the drain completed the review, so the next tick's 5.5
  discovery re-admitted the identical head SHA (generation bump +
  publication reset), the completed row was suppressed by the
  stale-generation guard, and the worker re-ran the same SHA every tick
  with no sticky comment ever published. Dedup now holds from completion
  onward and the next tick publishes the run it was built for. Closes
  #333.

## [1.0.0] - 2026-08-08

### Added

- **Scheduler leadership.** A single leader is elected per host with
  TTL-based leases. Followers defer to the leader's tick, eliminating
  the dual-tick race that v0.1.0 silently tolerated.
- **Bounded concurrency.** Up to N workers run in flight at once per
  cron tick (bounded-across-the-tick), and a single repository never
  runs two workers concurrently. Replaces the host-wide tick lock.
  Closes #107. Closes #109.
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
- **SQLite state backend switch.** migrate-state --to-sqlite imports
  the JSON queue into the SQLite store and flips state_backend to
  sqlite in the operator's config. Closes #110.
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
  published in the GitHub wiki.
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
  stderr. Closes #134.
- **Supervisor stdout capture.** The supervisor now pipes worker stdout
  into the bounded transcript alongside stderr (byte-interleaved, one
  shared writer) instead of discarding it. Previously worker stdout was
  sent to `/dev/null` and only stderr was captured. Closes #126.
- **Supervisor cancel/DONE race.** The daemon-side protocol loop drains
  the worker's `DONE` frame before firing the cleanup cancel, so a
  worker that exits 0 cleanly is no longer reported as cancelled. The
  cancel path still wins when fired while the worker is alive and no
  DONE is pending. Closes #130.
- **Supervisor hidden-command dispatch.** The hidden
  `__worker-supervisor` token is now matched only as the first argument
  after the binary name, not anywhere in `argv`, so a copy-pasted
  debugging recipe can no longer accidentally drop a normal CLI
  invocation into supervisor mode. Closes #129.
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
  before the PGID is confirmed. Closes #125.
- **Supervisor frame length validation and partial-header reads.** The
  supervisor's stdin readers now validate the frame length against
  `MAX_FRAME_BYTES` before allocating, and use `read_exact` for the
  4-byte header so a partial read can no longer be parsed as a length
  prefix. A malformed `0xFFFFFFFF` header or a truncated header from a
  crashed/hostile worker is rejected without a multi-GB allocation.
  Closes #127. Closes #128.

### Security

- **Adversarial isolation tests.** Escape, leak, network, and
  cancellation tests verify the executor cannot reach the host
  system, leak secrets into the worker environment, or escape the
  network policy.
- **OCI isolation end-to-end.** The executor enforces the isolation
  policy at the container boundary: a positional-engine rule requires
  every engine flag to appear before the image, and the adversarial
  suite runs against the real executor, not a mock.
- **Network policy.** Outbound traffic from the executor is
  allowlist-only by default.
- **Mount and secret allowlist.** The executor refuses to mount
  non-allowlisted paths or load secrets outside the explicit allowlist.

### Known Limitations

- **Environment-dependent integration tests.** A handful of tests
  expect the `caduceus` binary on the default `PATH` and a healthy
  `doctor` run. CI runs them in a controlled environment; on a bare
  host, put the binary on `PATH` first. These are environment
  dependence, not regressions.
- **Linux is tier-1.** macOS builds and runs, but is not exercised in
  CI before a release. Windows is not supported.

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
- An operator migration guide, later superseded by the wiki's
  State-Recovery page.

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

[Unreleased]: https://github.com/barkley-assistant/caduceus/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/barkley-assistant/caduceus/releases/tag/v1.0.0
[0.1.0]: https://github.com/barkley-assistant/caduceus/releases/tag/v0.1.0
