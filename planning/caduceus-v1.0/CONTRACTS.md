# Caduceus v1.0 — Normative Contracts

> This is the single normative specification for Caduceus v1.0. It
> supersedes the v0.1 contract on every surface named here. The v0.1
> planning tree is an immutable implementation archive, not v1.0
> authority.

## Goal and scope

Caduceus v1.0 is a correctness-and-scale release. It first makes the
shipped worker, Git, state, and release paths match their public
contracts. It then adds durable state, bounded single-host concurrency,
daemon-owned repositories, and isolated execution. Every pull request
remains subject to human review and merge.

The normative words MUST, MUST NOT, SHOULD, and MAY have their ordinary
requirements meaning. Requirement IDs are stable. Task packets and
acceptance evidence cite these IDs.

## Plan and evidence integrity

### PLAN-001 — Draft catalog safety

`task-manifest.json` declares whether the catalog is `draft` or
`active`. While it is `draft`, the controller MUST NOT select, start,
or complete implementation work. Activating the catalog requires a
task packet for every phase and a progress entry for every task.
An active phase MUST contain at least one task.

### PLAN-002 — Acceptance evidence

Every task acceptance check has a stable ID. Its handoff maps each ID
to a meaningful command or procedure, observed result, and durable
artifact or test reference. Empty values, placeholders, `N/A`, and `-`
do not count. A required check that is missing, failed, deferred,
stubbed, or contradicted prevents task and phase completion.

### PLAN-003 — Independent review

Work designated for human review remains `in_progress` until an
independent reviewer records the implementation actor, reviewer,
reviewed commit, decision, and external approval provenance. State
recovery, worker process lifecycle, executor isolation, and the release
canary require human review. The normalized actor and reviewer
identities MUST differ. The review artifact names the exact
implementation handoff and a 40- or 64-hex-character reviewed commit.

### PLAN-004 — Sealed historical implementation tree

The manifest records a deterministic digest of the complete
`planning/caduceus-v0.1/` tree. Validation hashes sorted relative paths
and file bytes, excluding only generated `__pycache__/`, `*.pyc`, and
`.progress.lock` artifacts. A mismatch is a safety stop and the v0.1
tree is never edited to repair it.

### PLAN-005 — Public readiness audit and implementation boundary

Phase 00 publishes six cross-linked, reproducible attachments: public capability
inventory, production-path reachability, operator journeys, fault injection,
requirement evidence, and public gap register. Capability states are
`working-production`, `integrated-not-proven`, `stub`, `fake-only`, `planned`,
or `contradicted`. Every gap has exactly one existing task/acceptance owner or
one approved deferral. Phase 00 routes gaps and MUST NOT claim they are fixed.

Phase 01 executes the installed-path walking skeleton. Baseline CI and fixture
self-tests must pass. Expected implementation failures may remain only when they
are deterministic, fully evidenced, and mapped to one later task/acceptance ID.
Fixture failures, unexpected or unowned failures, and silent success block the
phase. After Phase 01, plan refinement stops and implementation begins. Later
discoveries become evidence-backed issues or tasks, not speculative replanning.

## Baseline CI

### CI-001 — Continuous integration first

Phase 01 establishes CI before production implementation changes. On
every pull request and push to `main`, GitHub Actions runs:

```text
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
pytest -q tests/hermes_plugin_test.py tests/bridge_test.py
python3 -B planning/caduceus-v1.0/tools/validate_plan.py
```

The Rust gate runs on the pinned MSRV and stable Rust. Required-check
names and artifact-retention behavior are documented. New regression
jobs become required when their corresponding implementation lands;
the baseline workflow MUST remain green while the fixtures are built.

## V0.1 carryover debt

The full catalog MUST represent each debt ID below with task and
acceptance IDs. Phase 00 records and routes the debt; it does not claim
to implement runtime or state corrections.

- `DEBT-STATUS`: correct the status exit-code defect under Phase 02
  runtime correctness.
- `DEBT-RETENTION`: implement backup retention under Phase 03 SQLite
  migration and recovery.
- `DEBT-MSRV`: record the stale v0.1 packet as a Phase 00 historical
  deviation without editing the archive.
- `DEBT-ATOMIC`: establish the shared `install::atomic_write` primitive
  under Phase 03 as migration and recovery foundation.

### CI-002 — Reusable system fixtures

CI provides reusable Wiremock GitHub, disposable local Git origin,
real release-binary, crash-point, and process-tree fixtures. Fixtures
MUST be hermetic and MUST NOT require production credentials.

The pinned Hermes fixture also runs install `--enable`, records the external
restart prerequisite, setup, capability-present cron install, doctor, status,
and manual run; it repeats setup/cron and exercises capability absence. Its
report records command, exit, structured category, artifact, and gap owner.

### CI-003 — Commit policy

Every commit and merge commit MUST follow
[Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/)
as `<type>(<scope>): <description>`. Type and the required non-empty scope are
lowercase. The imperative description has no trailing period, and the complete
subject is at most 80 characters. `feat(lang): add Polish language example` is
an example, not a required type or scope. CI validates every PR commit when
commits are preserved, or the squash title when squash merge is enforced.

## Installation and Hermes lifecycle

### INSTALL-001 — Production configuration bootstrap

`Config::load` is complete and fail-closed. A set `$CADUCEUS_CONFIG` is
authoritative: missing, unreadable, or invalid content is an error with no
fallback, and setup never invents that file. Otherwise it checks the
`caduceus:` section in `$HERMES_HOME/config.yaml`, then the section in the XDG
standalone file. Empty or relative `$HERMES_HOME` values are rejected. A valid
source is validated and never overwritten.

Only explicitly invoked setup may create configuration, and only when no source
exists. It atomically adds a minimal non-secret `caduceus:` section to
`$HERMES_HOME/config.yaml`, preserving unrelated YAML and metadata, or creates
the file as mode `0600`. Rewrite preserves owner and mode, never widens
permissions, uses a mode-`0600` temporary file, cleans it after every outcome,
and redacts unrelated YAML secret values from diagnostics and evidence.
Interrupted setup is safely retryable. Generated configuration is immediately
loadable. The default worker is the user-owned
`$HERMES_HOME/caduceus/worker-bridge.py`.

### HERMES-001 — Transactional scheduling

Before wrapper mutation, scheduling performs a bounded, well-formed
`ctx.dispatch_tool('cronjob', {'action':'list'})`, locates a stable Caduceus job
marker, and snapshots wrapper bytes/mode plus the matching job. Malformed,
denied, timed-out, EOF, crashed, duplicate, or foreign-name-collision responses
are errors and MUST NOT be coerced to an empty list.

After create, update, or remove returns an error or ambiguous outcome, the
plugin re-lists and reconciles. The exact intended state is success. Unchanged
state
retains or restores the wrapper. Divergence compensates both job and wrapper to
their prior state. If exact rollback is impossible, the operation returns
nonzero `NeedsAttention` with manual-recovery evidence and never success.
Install, update, remove, and uninstall recovery are idempotent across a crash at
every wrapper/job boundary. Wrapper and job are never left mutually inconsistent
without surfaced `NeedsAttention`. The plugin never accesses `cron/jobs.json`.

### HERMES-002 — Diagnosable host health

Doctor checks binary, configured bridge and harness executables, required
provider secret-name presence, configuration, cron capability, registration,
and gateway delivery separately. Secret values are never inspected or emitted,
and readiness performs no provider network call. Configuration/runtime defects
exit 1. A missing harness or provider sentinel, host capability, or external
prerequisite exits 2. Categories are `host-capability-unavailable`,
`gateway-inactive`, `config-incomplete`, and `daemon-defect`. The plugin cannot
restart the external gateway and reports the exact operator action. No failed or
unsupported capability is reported as success.

## Production surface quality

### QUALITY-001 — Shipped integrations are production-ready

`plugin.yaml`, `__init__.py`, `_runtime.py`,
`plugin-assets/worker-bridge.py`, `plugin-assets/caduceus-pulse.sh`,
`skills/caduceus/SKILL.md`, generated wrapper/configuration text, and every
shipped hook or command integration contain no TODO/FIXME, Rust placeholder
macro, deliberate stub,
not-implemented marker, future-task or task-ownership comment, or production
message that says implementation is pending. Shipped execution uses no fake or
test-only hook path and exposes no development-only manifest field or command.

Production comments and docstrings explain a runtime invariant, safety reason,
supported behavior, or operator action in the established voice. Errors are
actionable and operator-facing. Generated scripts use only accurate production
comments such as do-not-edit notices, strict shell behavior, deterministic
paths, safe quoting, and private modes. Hooks are idempotent, transactional, and
capability-aware. A targeted scanner and actual installed lifecycle block Phase
02 on any hit unless an explicit production rationale is allowlisted; the
preferred allowlist is empty. Planning and tests may retain task and fixture
language, but production surfaces may not.

## Runtime correctness

### RUN-001 — One production worker path

The tick invokes one production worker adapter. That adapter MUST:

- build the sanitized worker environment defined by the v0.1 worker
  contract, including the required `CADUCEUS_*` values;
- execute the configured bridge under supervision;
- accept results only from `<worktree>/worker-result.json`;
- validate the result and archive it before finalization; and
- reject a result when the worker exits nonzero.

The child receives exactly these daemon-owned variables:

- `CADUCEUS_ISSUE_NUMBER`
- `CADUCEUS_ISSUE_TITLE`
- `CADUCEUS_ISSUE_BODY`
- `CADUCEUS_ISSUE_REPO`
- `CADUCEUS_ISSUE_LABELS_JSON`, encoded as a JSON array
- `CADUCEUS_WORKTREE_PATH`
- `CADUCEUS_RUN_ID`
- `CADUCEUS_CONTEXT_JSON`
- `CADUCEUS_BRANCH_NAME`, containing the daemon-owned expected branch

The optional inherited allowlist defaults to `PATH`, `HOME`, `USER`,
`SHELL`, `LANG`, `LC_ALL`, `TERM`, and `TMPDIR`, plus names matching
`OPENAI_*`, `ANTHROPIC_*`, `OPENROUTER_*`, and `OPENCODE_*`. An entry is
an exact portable environment name or a single terminal `*` prefix.
Empty entries, `=`, NUL, and any other wildcard placement are invalid.
GitHub credential names are denied even when configured. Logs record
names and redacted presence only, never values.

On exit 0 the bridge MUST leave exactly one result at
`<worktree>/worker-result.json` with this top-level shape:

```json
{
  "status": "success",
  "summary": "Non-empty Markdown summary",
  "commit_message": "fix(component): description",
  "pull_request_title": "fix(component): description",
  "artifacts": {"optional-name": "any JSON value"}
}
```

The file is at most 1 MiB and rejects unknown top-level fields. Required
strings are trimmed, nonempty, and NUL-free. `summary` is at most 64
KiB; commit and pull-request titles are at most 256 characters. The
pull-request title is one line without control characters. A commit
message may contain newlines but no other control characters. Artifact
keys are nonempty, control-free, at most 128 characters, and limited to
100 entries. Investigation uses the same stable schema but ignores the
commit and pull-request title values after validation.

Unit-test-only launch helpers MUST NOT define behavior that production
bypasses.

### RUN-002 — Deadline and process-tree cleanup

Worker execution has one hard deadline. Timeout, daemon cancellation,
and supervisor failure trigger a bounded TERM, descendant rediscovery,
KILL, and reap sequence for the complete process tree. Cleanup records
and distinguishes daemon PID, supervisor PID, worker PID and process
group, and process-start identity. Strong process identity is required
before signalling a reused numeric PID.

Supervisor EOF, supervisor crash, malformed protocol, timeout,
nonzero worker exit, invalid result, and cleanup failure are distinct
operator-visible outcomes. No outcome may hold scheduler ownership
indefinitely or leave a known descendant unreaped.

### RUN-003 — Bounded transcripts

Worker stdout and stderr flow through one size-bounded transcript
writer. The handoff and queue history report truncation and transcript
write failures. A transcript failure MUST NOT silently turn an invalid
worker run into success.

### RUN-004 — Hardened Git execution

All Git commands use one cancellable, timeout-bound runner. Status and
path output uses NUL delimiters and native path types; whitespace and
non-UTF-8 names MUST NOT be split or discarded. Worktree containment
is checked after path normalization and symlink resolution. Hooks and
ambient configuration MUST NOT bypass the daemon's intended command
behavior.

GitHub PAT authentication uses a noninteractive, ephemeral credential broker or
askpass endpoint. The token travels through an inherited anonymous descriptor,
or an equivalent channel that never places it in argv, URLs, Git configuration,
or durable files. Token values never appear in logs, output, or evidence.

### RUN-005 — CLI status codes

`caduceus status` returns 0 after successfully reading and rendering
state, 2 when state is missing, and 1 when state or queue data is
corrupt. Shell-level tests assert the process exit status.

## State, migration, and recovery

### STATE-001 — SQLite is the v1.0 runtime store

SQLite is the only active v1.0 runtime backend. JSON is retained only
as an import, export, and backup format. The database separates current
issue generation and state, attempts and events, claims and leases,
finalization checkpoints, repository and provider circuit state, and
archive and compaction metadata.

Each transaction preserves queue and claim invariants. Schema versions
newer than the running daemon are rejected without mutation.

### STATE-002 — Explicit safe migration

`caduceus migrate-state --to sqlite` acquires the daemon lock, creates
and validates a source backup, imports in one transaction, verifies
invariants, and activates SQLite only after all checks pass. Failure
leaves v0.1 state active and unchanged. Success retains the validated
source backup. V1.0 has no permanent dual-write or downgrade mode.

> **v0.1 ↔ v1.0 command split.** The currently shipped
> `caduceus` binary exposes `caduceus migrate-state --from <legacy.json>
> [--dry-run]`, a JSON-envelope import used by `MIGRATION.md` and
> `docs/state-recovery.md` today. The v1.0 `--to sqlite` form is
> **planned**, not present in any shipped binary. It becomes available
> only when Task 3.3 lands the implementation and ships in a v1.0
> release. Until then, treat the two flags as different subcommands:
> `--from <legacy.json>` for cross-format imports today;
> `--to sqlite` for the v1.0 cutover when 3.3 ships. Operator docs
> must not present `--to sqlite` as a current capability.

Unknown legacy statuses, guessed ticket types, and `InProgress`
records without valid claims are rejected unless the operator supplies
an explicit mapping accepted by the command.

Dry-run produces a deterministic per-record disposition report and audit
artifact before mutation. Allowed dispositions are import, skip, or preserve as
`NeedsAttention`; the operator explicitly confirms the report. Unknown records
and claimless `InProgress` records are preserved as `NeedsAttention` with their
original raw evidence. Migration MUST NOT fabricate a claim. Configuration v2
is validated first; state activation and configuration installation form one
recoverable cutover. Failure rolls both surfaces back to their prior active
versions.

### STATE-003 — Supported recovery commands

`caduceus recover-state` and the metadata recovery command operate
under the daemon lock, archive corrupt input, validate repaired data,
install atomically, and emit a corruption manifest. Documentation MUST
NOT instruct operators to edit daemon-owned state, metadata, claim, or
transcript files in place.

### STATE-004 — Generations and retention

A reopened issue and `caduceus queue reprocess <issue>` create a new
trigger generation. Label changes may retarget work before it is
claimed. A relevant label change after execution begins creates a
subsequent generation rather than mutating the active attempt.

Retention and compaction preserve active claims, finalization
checkpoints, corruption evidence, and the configured audit window.

## Durable finalization and review lifecycle

### FINAL-001 — Durable checkpoints

The finalization sequence is:

```text
ResultValidated -> Committed -> Pushed -> PrCreated -> Commented
  -> AwaitingReview -> Done
```

Each checkpoint is committed before beginning the next externally
visible action. Recovery resumes from the last durable checkpoint and
uses idempotency keys or remote reconciliation so a crash does not
duplicate commits, pushes, pull requests, comments, or issue updates.

Each stage stores a durable operation ID derived from the run and stage before
its external effect. Branches, pull requests, and comments carry exact remote
markers used for lookup. An unavailable remote leaves the stage pending;
conflicting markers transition the generation to `NeedsAttention`.

### FINAL-002 — Human merge lifecycle

Creating a pull request leaves the issue open and the queue entry in
`AwaitingReview`. A merged pull request transitions the generation to
`Done`; issue closure occurs through a closing keyword or explicit
reconciliation. A pull request closed without merge transitions to
`NeedsAttention`. Caduceus MUST NOT auto-merge in v1.0.

## Scheduling, repositories, and failures

### SCHED-001 — Bounded single-host concurrency

The full-tick global lock is replaced by short transactional scheduler
leadership and per-issue leases with fencing tokens. Concurrency is
bounded by `worker_parallelism`, whose default is `1`. A repository has
at most one active mutation path. The scheduler provides backpressure,
graceful drain, lease renewal, and safe recovery of definitively dead
workers.

### SCHED-002 — Failure control

Infrastructure failures are counted and aged separately from worker
attempts. Bounded exponential backoff and provider/repository circuit
breakers prevent infinite retries. Exhausted or persistently degraded
work transitions to `NeedsAttention` with actionable evidence.

Circuit state is persisted independently for each provider and repository. The
defaults are three consecutive infrastructure failures, exponential delays of
30 seconds, 2 minutes, and 10 minutes, a 30-minute open interval, one half-open
probe, reset after one successful probe, and a 24-hour maximum degraded age
before `NeedsAttention`. Configuration may make these values more conservative.
An injected clock makes restart, open, half-open, reset, and elapsed-age tests
deterministic.

### REPO-001 — Daemon-owned repositories

The daemon maintains bare mirrors in daemon-owned storage and creates
disposable worktrees for attempts. Runtime operation MUST NOT depend on
an operator checkout being present or clean. Mirror updates,
worktree creation, mutation, and cleanup use the hardened Git runner.

State, mirror, backup, and transcript directories are mode `0700`; private
files are `0600`. A restrictive umask applies without destroying executable or
source modes inside private worktrees. Atomic replacement preserves the intended
mode. Symlinked storage roots are refused.

### GH-001 — Authentication and discovery

V1.0 retains PAT authentication and requires an explicit repository
scope. Discovery uses bounded incremental polling and may use ETags.
Tokens are never exposed to workers, transcripts, Git command output,
or public GitHub text.

PAT-backed Git uses the ephemeral credential transport defined by `RUN-004`.

#### Supported GitHub API endpoints

V1.0 supports exactly two GitHub API endpoint families:

1. **GitHub.com** — the public SaaS at `https://api.github.com`.
2. **GitHub Enterprise Server (GHES)** — a self-hosted instance
   reachable at a configured HTTPS URL. GHES is supported because
   operators run it; arbitrary non-GitHub REST endpoints, custom
   GitHub forks, and proxy shims that present a GitHub-shaped API but
   are not GitHub are not supported and MUST be rejected at
   configuration load.

The validation rules Task 5.5 enforces:

- `api_base` MUST be either the literal
  `https://api.github.com` or an `https://` URL whose host matches
  the GHES host pattern documented in
  `docs/configuration.md` §"`api_base`". Anything else — http://,
  arbitrary subdomains, custom CA bundles, corporate proxies with
  path prefixes, Bitbucket-style REST surfaces, etc. — is rejected
  with a configuration error at `Config::load`.
- The configuration MUST NOT rely on string matching against
  `comment_forbidden_strings` or any other forbidden-list filter to
  detect a non-GitHub endpoint. Endpoint validation is a positive
  allowlist of the two known forms, not a negative string check.
- The host tier rule (Linux tier-1, macOS supported, Windows not a
  target) applies equally to the GHES host. An operator who runs
  GHES on an unsupported host inherits the unsupported host's
  caveats.

## Executor isolation

### EXEC-001 — Executor abstraction

Worker execution is selected through an executor interface. V1.0
provides an OCI CLI executor compatible with configured Docker or
Podman executables and a trusted-host executor.

### EXEC-002 — Isolation defaults and boundary

OCI isolation is the default for new installations. Each run declares
its image, read/write worktree and result mounts, network profile,
resource limits, and per-run provider-secret grants. Network is
disabled unless an explicit profile enables it. Daemon state, GitHub
credentials, and host repository storage MUST NOT be mounted or passed
to the worker.

Trusted-host execution requires explicit opt-in acknowledging reduced
containment. Upgrade documentation preserves existing installations
until the operator selects and validates an executor mode.

Workers are deliberately Git-less. The daemon owns every Git operation. OCI
mounts the worktree read/write while masking `.git`; mirrors and object stores
are never mounted. After container exit, the daemon validates edits before any
Git operation.

OCI secret access is default-deny. Only explicitly configured per-run secret
names are granted; broad prefix inheritance applies only to explicit
trusted-host mode and never overrides OCI policy. Values are written to a
mode-`0600`
ephemeral environment or secret file, and only its path reaches the OCI CLI.
The file is deleted after startup failure, normal exit, cancellation, and
startup orphan recovery. Values never appear in argv, logs, or evidence.

The OCI baseline runs as a configured non-root UID/GID, drops all capabilities,
sets no-new-privileges, uses a read-only root filesystem, and declares every
writable mount or tmpfs. It mounts no device or container-engine socket. Images
are pinned by digest and follow an explicit pull policy.

Every container has stable daemon, run, and issue labels. Startup inventories
and reconciles orphaned containers with bounded stop, kill, and remove steps.
Engine unavailability is operator-visible and leaves reconciliation pending.
Recovery is crash-safe at create, start, wait, stop, and remove.

## Public configuration and compatibility

### CONFIG-001 — Configuration schema v2

Configuration schema v2 adds only the capabilities required here:

- SQLite state location;
- `worker_parallelism`, default `1`;
- executor mode and OCI settings;
- explicit repository scope; and
- circuit-breaker and retention settings.

Migration validates the complete resulting configuration before atomic
installation. The v0.1 worker environment and result schemas remain
compatible unless a later authorized contract revision identifies an
unavoidable change.

### CONFIG-002 — Toolchain

Rust 2021 and MSRV 1.97 remain required. `Cargo.lock` is committed and
CI uses `--locked`. SQLite support may add a bundled SQLite dependency;
executor support shells out to the configured OCI CLI. Every dependency
change is isolated and justified by its task packet.

## Acceptance and release

### ACCEPT-001 — Full-system regression suite

The v1.0 release suite includes all ten integration scenarios required
by the v0.1 plan and asserts exact GitHub mutation counts. It also
covers:

- the reference bridge through the production worker adapter;
- timeout, detached descendants, supervisor crash and EOF, malformed
  protocol, and nonzero exit with a result file;
- a crash after every durable finalization checkpoint;
- reopen, retarget, reprocess, merge, and closed-without-merge flows;
- migration success, every documented crash point, rollback,
  corruption recovery, retention, and compaction;
- concurrent workers, repository exclusion, lease fencing, drain,
  backpressure, and circuit breakers;
- whitespace, newline, non-UTF-8 and symlink Git paths, hooks,
  cancellation, and timeouts; and
- OCI mounts, credentials, network, and resource boundaries.

### ACCEPT-002 — Host and release evidence

The suite exercises the real Hermes Agent v0.18.2 lifecycle in
addition to fast fake-context tests. The final canary runs the built
release binary against Wiremock and a disposable local Git origin and
records exact side effects. Recovery, worker lifecycle, executor
isolation, and canary evidence receive independent human approval.

### ACCEPT-003 — Installed-path truth

The pinned-host lifecycle uses an isolated disposable real Hermes Agent v0.18.2
gateway under a temporary `$HERMES_HOME`. An external harness performs restart
and delivery observation. It installs with `--enable`, runs setup twice, tests
cron presence and absence, doctor, status, manual run, scheduled delivery,
update/rebuild, cron removal, and uninstall preservation without user state.

The canary installs and enables the exact candidate plugin commit/archive and
binary, recording their commit and SHA-256 before human review. It restarts the
gateway externally, runs setup twice, proves exactly one registered job, doctor
exit 0, and expected status. A manual dry run records a report with zero Git or
GitHub mutations. A scheduled non-dry code issue ends in `AwaitingReview` with
the issue open, exactly one commit, branch push, pull request, and public
comment, and zero merges, issue closes, or unexpected label mutations. Evidence
records
the request log, object IDs, run ID, PR URL, transcript/report, and cleanup.

Documentation and release evidence MUST NOT claim end-to-end readiness until the
installed path passes; blocked or unsupported capabilities are labeled honestly.

## Risk register

- CI false confidence: use the real binary and production adapter in
  system tests.
- Worker outlives daemon: enforce deadline and identity-safe
  process-tree cleanup.
- Migration loses state: validate backups, import transactionally, and
  activate atomically.
- Crash duplicates GitHub effects: persist checkpoints and reconcile
  remote state.
- Parallel workers collide: use fenced leases and repository mutation
  exclusion.
- Host worker reads secrets: default to OCI and grant secrets
  explicitly.
- Evidence overstates completion: require stable acceptance IDs and
  reject incomplete evidence.

## Explicit v1.x deferrals

The following are outside v1.0:

- GitHub App authentication and webhooks;
- policy-gated or unattended auto-merge;
- multi-host coordination and leader election;
- custom bridge composition or a `bridge.d/` plugin chain;
- native dashboards; and
- automated release tooling.

No deferred item is required to uphold a v1.0 promise.

## Definition of done

V1.0 is done only when all active catalog tasks and phase gates are
complete, every requirement has passing acceptance evidence, the
canonical CI and release canary are green, required independent reviews
are approved, operator migration and recovery documentation is current,
and the v0.1 archive is byte-for-byte unchanged.

## Contract revision control

`CONTRACTS.md` is sealed by `contracts_sha256` in
`task-manifest.json`. A mismatch is a safety stop. Only an explicitly
authorized revision may update this file and its digest. The revision
record identifies the approver, rationale, affected requirements and
planning surfaces, and required re-verification. All affected v1.0
surfaces are updated together. The v0.1 archive and its digest are never
modified.
