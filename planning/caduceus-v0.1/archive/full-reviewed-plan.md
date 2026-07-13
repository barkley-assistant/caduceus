# Caduceus v0.1 Implementation Plan

> **SDD workflow:** Each task is a self-contained unit. A subagent picks up one task at a time with this document as context. Every task follows RED (write failing test) → GREEN (implement) → REFACTOR. Never skip the RED step.

**Goal:** Build a self-hosted Rust daemon, shipped as a **Hermes plugin**, that polls GitHub for labeled issues, manages an atomic claim queue, provisions isolated git worktrees, spawns a user-configurable AI harness via a Python bridge, enforces a hard worker timeout, and finalizes the result as a branch + push + PR + issue close.

**Architecture:** Plugin-primary, daemon-binary. A small root Python adapter registers the Hermes skill, status command, setup/doctor CLI, and explicit cron lifecycle; the Rust daemon owns process lifecycle, IO, atomicity, GitHub work, and observability. Setup seeds a user-owned harness bridge. The bridge reads `CADUCEUS_*` env vars, invokes a configured harness (OpenCode, pi, codex, claude-code, anything), edits files in the worktree, writes `worker-result.json`, and exits with a code. The daemon never sees an LLM API key or token.

**Tech Stack:**
- Rust 2021 edition (≥ 1.75)
- `tokio` (async runtime)
- `reqwest` (HTTP client with ETag support)
- `serde` + `serde_yaml` (config parsing)
- `clap` (CLI subcommands)
- `git2` (libgit2 bindings for worktree management) — alternative: shell out to `git`
- `tracing` + `tracing-subscriber` (structured logging)
- `fs2` (POSIX file locking via `flock`)
- `ulid` (run IDs)
- Reference worker: A user-editable Python bridge (`plugin/worker-bridge.py`) that wraps OpenCode + Gentle-AI by default. Users fork the bridge to plug in pi, codex, claude-code, or any other harness.

---

## Binding v0.1 Corrections and Acceptance Overlay

> **Preservation and precedence rule:** The original 46-task RED/GREEN/REFACTOR playbook remains below in full. This overlay records the review corrections without discarding that implementation detail. Engineers execute the original task steps and inline tests, augmented by the corresponding overlay requirements. Where an original prose statement, signature, schema, endpoint, placeholder stub, or inline test conflicts with this overlay, the overlay is authoritative and the conflicting fragment must be updated in that same task. Unmentioned original detail remains authoritative. A task is not complete until both its original acceptance steps and its overlay acceptance criteria pass.
>
> This overlay is intentionally inside the same planning document so a fresh subagent has one source of truth. It is not a replacement plan and must not be extracted into a second implementation track.

### What is normative

When two statements differ, use this order of authority:

1. Non-negotiable invariants and canonical public contracts in this overlay.
2. The exact lifecycle, failure classification, and orchestration order in Amendments 3.1–3.4 and 7.0–7.1.
3. The applicable amendment's acceptance criteria and named edge cases.
4. Unchanged prose in the original task body.
5. Original inline code and test snippets.

Inline snippets are implementation aids, not frozen production code. Do not copy a snippet that uses a superseded field, endpoint, environment variable, error variant, or signature. Rewrite that test around the normative behavior and public contract instead. Local helper types and private signatures may evolve during implementation; changing a serialized schema, CLI, environment variable, state transition, public signature listed in the overlay, or cross-module ownership boundary requires a plan update first.

An agent may choose a different internal implementation only when all named failure paths and forbidden side effects remain covered. If a task cannot meet its acceptance criteria without changing a higher-authority contract, the task is blocked and must not silently redesign that contract.

### Goal and scope

Caduceus is a Unix single-host, one-shot Rust daemon shipped as a Hermes plugin. Each invocation polls GitHub for open issues carrying one of two configured trigger labels, atomically queues at most one unit of work, provisions an isolated git worktree, runs a user-editable harness bridge under a hard process-tree timeout, and finalizes a successful code result as a commit, push, pull request, and issue close. Investigation results are posted as findings without a code commit or PR. Linux is the tier-1 release platform; macOS is supported through the same Unix supervisor/session contract.

The daemon owns GitHub credentials, polling, state, claims, worktrees, prompts, environment construction, process groups, transcripts, heartbeats, git operations, public-text validation, retries, and status metadata. The bridge owns only translation from `CADUCEUS_*` inputs to a harness command and propagation of the harness exit code.

### Non-negotiable v0.1 invariants

1. A nonblocking exclusive lock on `<state_dir>/daemon.lock` covers an entire tick. A second cron invocation exits 0 without polling or claiming.
2. Queue and metadata files use same-directory temporary files, `fsync`, and atomic rename. Malformed `state.json` is never replaced with an empty state.
3. Claim creation, queue transition, claim release, and retry transition are `StateStore` operations. Queue helpers never construct claim paths from raw issue strings.
4. A claimed issue leaves `InProgress` through exactly one terminal operation: `complete`, `complete_investigation`, `retry_or_fail`, or `skip`. Every operation removes its claim.
5. The daemon owns the git branch name. Worker output cannot select a ref.
6. The worker is spawned in a new Unix session behind the internal Rust worker supervisor. Timeout, SIGINT, SIGTERM, and daemon-parent death kill the whole worker session and await output-drain tasks before cleanup.
7. Rust, not Python, owns heartbeat creation and removal.
8. No GitHub credential resolved or held by the daemon is injected into the worker environment or command. The child uses `env_clear()` followed by an explicit allowlist plus documented `CADUCEUS_*` variables. Same-user filesystem access is not an OS sandbox and is documented separately.
9. All public GitHub text—comments, PR title, and PR body—passes the public-voice check before any API mutation.
10. Finalization is idempotent across partial failures: existing remote branches and open PRs are detected and reused.
11. A rate-limit observation is persisted before returning. No later tick performs GitHub calls before the persisted reset time.
12. No-argument CLI invocation is exactly equivalent to `caduceus run`.

### Toolchain and dependencies

- Rust 2021, MSRV 1.75
- Runtime: `tokio`, `tokio-util`, `reqwest`, `serde`, `serde_json`, `serde_yaml`, `clap`, `tracing`, `tracing-subscriber`, `tracing-appender`, `thiserror`, `fs2`, `ulid`, `chrono`, `regex`, `which`, `shellexpand`, `sha2`, `hex`, `filetime`, `walkdir`, `libc`
- Git implementation: shell out to the installed `git` executable. This avoids libgit2 credential divergence and uses the operator's existing SSH agent or credential helper. Every invocation uses argument arrays, never a shell string.
- Dev: `tempfile`, `wiremock`, `assert_fs`, `predicates`, `serial_test`
- Python bridge tests: `pytest`

Commit `Cargo.lock` and use `--locked` for CI, plugin, and release builds. The dependency resolver must pass the release suite on Rust 1.75; upgrading a crate in a way that raises MSRV is a documented compatibility change, not an incidental lockfile refresh.

### Canonical public contracts

#### Configuration

```rust
pub struct Config {
    pub poll_interval_seconds: u64,          // 120; must be > 0
    pub state_dir: PathBuf,                  // $HERMES_HOME/caduceus-state
    pub log_path: PathBuf,                   // <state_dir>/processor.log
    pub workdir_base: PathBuf,               // ~/projects
    pub watched_repos: Vec<String>,          // [] means discover via /user/repos
    pub worker_command: Vec<String>,         // resolved plugin default or explicit
    pub worker_timeout_seconds: u64,         // 3600; must be > 0
    pub http_timeout_seconds: u64,           // 60; must be > 0
    pub git_timeout_seconds: u64,            // 300; must be > 0
    pub transcript_max_bytes: u64,           // 10 MiB
    pub run_retention_days: u64,             // 30; must be > 0
    pub stale_run_hours: u64,                // 1; must be > 0
    pub max_retries_per_issue: u32,          // 3 total failed attempts; must be > 0
    pub retry_backoff_seconds: u64,          // 300; must be > 0
    pub ticket_label_code: String,
    pub ticket_label_investigation: String,
    pub feedback_author_allowlist: Vec<String>,
    pub comment_ignore_patterns: Vec<String>,
    pub comment_forbidden_strings: Vec<String>,
    pub worker_env_allowlist: Vec<String>,
    pub github_token: Option<String>,
    pub api_base: String,
    pub dry_run: bool,
}
```

`Config::load()` resolves `$CADUCEUS_CONFIG`, then `$HERMES_HOME/config.yaml` under `caduceus:` (`HERMES_HOME` defaults to `~/.hermes`), then `~/.config/caduceus/config.yaml` under `caduceus:`. Relative `HERMES_HOME` is rejected. `CADUCEUS_DRY_RUN` overrides YAML when its value is one of `1,true,yes`; `0,false,no` disables it; other values are errors. Paths expand only a leading `~`; no shell expansion is performed. In `worker_command` arguments only, the exact token `${plugin_root}` is replaced with the plugin root derived from the installed executable. No other `${...}` interpolation is accepted.

When `worker_command` is absent and the executable has the canonical `<plugin>/bin/caduceus` layout, the daemon uses `python3 $HERMES_HOME/caduceus/worker-bridge.py` if setup created that user-owned file. Standalone/noncanonical installs must set `worker_command`; absence is a validation error. Tests use `Config::test_defaults(root: &Path)`, never a host-dependent `Config::defaults()`.

Token resolution is explicit config, `CADUCEUS_GITHUB_TOKEN`, `GITHUB_TOKEN`, then `gh auth token`. Empty values are ignored. Errors never include token contents.

#### Issue identity and queue schema

```rust
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IssueKey { pub owner: String, pub repo: String, pub number: u64 }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase { Queued, InProgress, Previewed, Done, Failed, Skipped }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketType { Code, Investigation }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizationStage { Committed, Pushed, PrCreated, Commented, InvestigationReady, InvestigationCommented }

pub struct FinalizationCheckpoint {
    pub run_id: String,
    pub branch_name: String,
    pub result_path: PathBuf,          // secure copy under state_dir/runs
    pub stage: FinalizationStage,
    pub commit_oid: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
}

pub struct QueueEntry {
    pub key: IssueKey,
    pub phase: Phase,
    pub ticket_type: TicketType,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub last_run_id: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub finalization: Option<FinalizationCheckpoint>,
    pub queued_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct QueueState { pub version: u32, pub entries: BTreeMap<String, QueueEntry> }
```

The map key is lowercase `owner/repo#number`, derived from a validated `IssueKey`; fields retain GitHub's canonical casing for display/API paths. Owner is 1–39 alphanumeric/hyphen characters, cannot begin/end with a hyphen, and repo is 1–100 `[A-Za-z0-9_.-]` characters other than `.` or `..`; number must be positive. Configured repositories deduplicate case-insensitively. Claim filenames are `sha256(lowercase_display_key).claim`; claim contents are JSON `{version,key,run_id,pid,process_start_identity,started_at,worktree_path}`. On Linux, process identity combines boot ID and `/proc/<pid>/stat` start ticks so PID reuse cannot preserve a stale claim; other Unix platforms use the strongest available process start/executable check and fall back conservatively to age.

After a worker result is validated, the daemon copies it atomically to `<state_dir>/runs/<run_id>.result.json` with mode `0600`. Checkpoint loading verifies `result_path` is a regular non-symlink beneath the canonical runs directory and revalidates its schema/hash. Once a code commit exists—or before an investigation comment is attempted—the queue entry receives a durable `FinalizationCheckpoint`. A later tick with a checkpoint resumes that exact branch/result/stage and does not invoke the worker or generate a new run ID. Stage advancement is persisted immediately after each idempotent side effect. Completion retains the checkpoint as audit data; queue reset refuses a checkpoint with a live/open PR unless `--force-finalization-reset` is explicitly supplied and confirmed in dry-run output.

Retry semantics use total worker-attributable failed attempts: with a budget of 3, failures one and two return to `Queued` with `next_attempt_at = now + retry_backoff_seconds`; failure three transitions to `Failed`. GitHub, git transport, local I/O, rate-limit, and operator-cancellation failures do not increment this budget. A still-open issue already in `Failed` is not automatically reset. Operators use the explicit queue reset command defined in Task 3.4; removing and re-adding a label alone does not bypass the budget.

Dry-run success transitions to `Previewed`. While dry-run remains enabled, rediscovery is a no-op. On the first non-dry tick, rediscovery atomically promotes a still-labeled `Previewed` entry back to `Queued`, so previewing never prevents the eventual real run.

#### Polling contract

The daemon does not consume GitHub's heterogeneous Events API. It discovers repositories with paginated `GET /user/repos?per_page=100&sort=full_name` unless `watched_repos` is configured, then performs one paginated open-issue query per URL-encoded trigger label: `GET /repos/{slug}/issues?state=open&labels={label}&per_page=100&sort=updated&direction=desc`. Results are merged by case-insensitive issue key. Pull-request objects are excluded by the presence of `pull_request`. Trigger labels are still verified from each returned object's label array rather than trusting the query alone. An issue present in both mutually exclusive result sets is reported as ambiguous and is not enqueued until a user removes one.

Every GET page has a persisted ETag entry in `<state_dir>/cache/http.json`. A 304 reuses the last successfully parsed body stored with that ETag. Cache writes are atomic. Invalid cache JSON or an invalid ETag drops only the affected cache entry and refetches unconditionally. The first tick processes current labeled issues; there is no historical event replay.

All requests set `User-Agent: caduceus/<version>`, `Accept: application/vnd.github+json`, and `X-GitHub-Api-Version: 2022-11-28`. All non-2xx/304 statuses become typed errors. `Link` pagination is followed within a configurable hard maximum of 20 pages per endpoint; exceeding it is an error rather than silent truncation.

#### Worker environment and result

The child receives exactly these Caduceus variables:

- `CADUCEUS_ISSUE_NUMBER`, `CADUCEUS_ISSUE_TITLE`, `CADUCEUS_ISSUE_BODY`, `CADUCEUS_ISSUE_REPO`
- `CADUCEUS_ISSUE_LABELS_JSON` (JSON array; the comma-separated variable is removed)
- `CADUCEUS_WORKTREE_PATH`, `CADUCEUS_RUN_ID`, `CADUCEUS_CONTEXT_JSON`
- `CADUCEUS_BRANCH_NAME` (daemon-owned expected branch)

The inherited allowlist defaults to `PATH`, `HOME`, `USER`, `SHELL`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, plus variables matching `OPENAI_*`, `ANTHROPIC_*`, `OPENROUTER_*`, and `OPENCODE_*`. GitHub credential names are denied even if users add them to the allowlist. Startup logs variable names and redacted presence only, never values. Because the worker normally runs as the daemon's OS user, it may still be able to read that user's credential files; operators requiring a hostile-worker security boundary must run the bridge in a separately configured container/user sandbox.

Allowlist syntax is an exact variable name or one terminal `*` prefix pattern such as `OPENAI_*`. Any other wildcard placement, empty entry, `=`, NUL, or nonportable variable name is a configuration error.

#### Filesystem permissions

The daemon creates `state_dir`, `runs`, `claims`, `cache`, and temporary worker-home/control directories with mode `0700`, and state, metadata, cache, claim, heartbeat, transcript, and dry-run files with mode `0600`. Existing paths that are symlinks, non-directories, group/world writable without the sticky-bit exception, or owned by another user are rejected. Atomic replacement reapplies the intended mode. Tests may skip ownership assertions only on platforms that cannot expose Unix metadata.

On exit 0 the bridge must leave `<worktree>/worker-result.json`:

```json
{
  "status": "success",
  "summary": "Non-empty Markdown summary",
  "commit_message": "fix(component): description",
  "pull_request_title": "fix(component): description",
  "artifacts": { "optional-name": "any JSON value" }
}
```

The file is limited to 1 MiB. Unknown top-level fields are rejected. Required strings are trimmed, non-empty, NUL-free, and limited to 64 KiB for summary and 256 characters for commit/PR titles. PR title is one line with no control characters; commit message may contain newlines but no other control characters. Artifact keys are nonempty, control-free, at most 128 characters, and limited to 100 entries. `artifacts` is `BTreeMap<String, serde_json::Value>` and its rendered PR section is escaped and size-limited. Investigation uses the same schema; `commit_message` and `pull_request_title` must still be present for schema stability but are ignored.

#### Finalization contract

The daemon creates `automation/issue-<number>-<run-id-lowercase>` before worker launch and exports it. Code success requires at least one tracked or untracked change other than daemon control files (`worker-prompt.md`, `worker-result.json`, dry-run reports). The daemon excludes those files from commits, commits all remaining changes, pushes `HEAD:refs/heads/<branch>`, finds or creates an open PR for that head/base, posts a completion comment if not already present, and closes the issue if still open. Each step treats an already-achieved state as success.

Investigation success posts a voice-checked findings comment derived from `summary` and leaves the issue open with the trigger label removed. It performs no commit, push, or PR creation.

Dry-run performs polling, claim, issue fetch, prompt creation, worker execution, result validation, and change inspection. It performs no commit, push, comment, label mutation, PR, or issue close. It writes `<state_dir>/runs/<run_id>.dry-run.md` before teardown.

#### State metadata and status

`<state_dir>/state_meta.json` contains schema version, tick start/finish/outcome, last HTTP status, next allowed poll time, reap time/count, rate-limit limit/remaining/reset, and last error. It uses the same atomic writer as queue state.

`caduceus status [--json]` loads configuration through the normal resolution chain, then reads that config's `state_dir`. It reports version, last tick timestamps/outcome, currently running worker and transcript, counts by queue phase, FIFO next head, recent errors, reap stats, and rate-limit data. Missing state yields a distinct nonzero diagnostic; corrupt state yields a nonzero diagnostic preserving the file. Heartbeats older than 90 seconds are stale, not live.

#### CLI contract

```text
caduceus                         # identical to `caduceus run`
caduceus run
caduceus status [--json]
caduceus worktree-gc [--older-than-days N] [--dry-run]
caduceus queue reset <owner/repo#number> [--dry-run]
caduceus migrate-state --from <path> [--dry-run]
```

Implement no-argument behavior by inspecting `args_os` and inserting `run` before Clap parsing; do not rely on a nonexistent/default-subcommand annotation. Successful `run` writes no stdout. Diagnostics go to stderr. `run` returns 0 for processed/idle/concurrent/cadence/rate-limit/cancelled outcomes and 1 for configuration, corruption, invariant, or unrecovered pipeline failures. `status` returns 2 for missing state and 1 for corrupt/unreadable state. Mutation/recovery commands require the daemon lock.

`__worker-supervisor` is a hidden internal mode dispatched before public command handling. It is never shown in help, never accepted from cron/plugin configuration, and refuses to start unless its inherited versioned control/status file descriptors are valid.

#### Hermes plugin compatibility contract

Hermes Agent v0.18.2 (`v2026.7.7.2`) is the minimum tested host for v0.1. The repository root is the installable Hermes directory-plugin root because `hermes plugins install barkley-assistant/caduceus` clones the selected repository root into `~/.hermes/plugins/caduceus/`. Installing only the historical `plugin/` subdirectory is unsupported: Hermes would move that subdirectory without the Rust workspace needed by setup.

The canonical Hermes-facing layout is:

```text
plugin.yaml
__init__.py
Cargo.toml
Cargo.lock
src/
skills/caduceus/SKILL.md
plugin-assets/worker-bridge.py
plugin-assets/caduceus-pulse.sh
bin/                         # generated by explicit setup; ignored by git
```

`plugin.yaml` uses only fields consumed by the v0.18.2 directory-plugin loader: `manifest_version`, `name`, `version`, `description`, `author`, `kind`, `requires_env`, `provides_tools`, and `provides_hooks`. Caduceus uses `kind: standalone`, no tools/hooks, and does not put defaults, file lists, binaries, lifecycle shell commands, profiles, or cron declarations in the manifest. Unknown manifest fields may be silently ignored by Hermes and are therefore rejected by Caduceus's contract test.

Root `__init__.py` is a small, standard-library-only Hermes adapter. Its `register(ctx)` explicitly registers:

- `ctx.register_skill("caduceus", <root>/skills/caduceus/SKILL.md, ...)`, resolvable as `caduceus:caduceus`; plugin skills are opt-in and are not automatic trigger rules.
- `/caduceus-status` through `ctx.register_command`; it invokes `<root>/bin/caduceus status --json` with an argument array, a short timeout, and bounded output, then returns a chat-safe diagnostic when setup has not built the binary.
- `hermes caduceus ...` through `ctx.register_cli_command`, with `setup`, `doctor`, `status`, `cron-install`, and `cron-remove` subcommands.

Plugin import/registration never compiles code, mutates config, creates cron jobs, or performs network access. `hermes caduceus setup` is the explicit build/install step: verify Rust/Cargo/Git/Python prerequisites, run `cargo build --release --locked --manifest-path <root>/Cargo.toml`, atomically install the resulting executable as `<root>/bin/caduceus`, create the configured state directories with secure modes, and seed the user-owned bridge at `$HERMES_HOME/caduceus/worker-bridge.py` (default `~/.hermes/caduceus/worker-bridge.py`) only when absent. If the shipped bridge template changes and the user copy differs, setup writes a sibling `.new` candidate and reports it; it never overwrites the user bridge. The daemon's plugin default points to this user-owned path. Standalone installs must set `worker_command` explicitly.

Hermes cron does not import `cron/*.yaml`. `hermes caduceus cron-install` atomically writes a generated Bash wrapper beneath `$HERMES_HOME/scripts/caduceus-pulse.sh`; the wrapper contains the absolute installed binary path and uses `exec <binary> run`. It then creates or reconciles exactly one named `caduceus` no-agent job equivalent to:

```text
hermes cron create "every 2m" --name caduceus --script caduceus-pulse.sh --no-agent
```

Reconciliation uses Hermes's registered `cronjob` tool through `ctx.dispatch_tool`, not direct edits to `cron/jobs.json`: zero matches creates, one match updates/reuses, and multiple matches fail with their IDs. `cron-remove` removes the recorded/matching job and wrapper idempotently. The gateway must be running (or a configured managed cron provider active) for scheduled jobs to fire. The daemon's own full-tick lock remains required because manual runs and multiple Hermes schedulers may overlap.

Hermes has no plugin uninstall hook. Operators run `hermes caduceus cron-remove` before `hermes plugins remove caduceus`; removal preserves `$HERMES_HOME/caduceus/`, the daemon state directory, user config, and repositories. `hermes plugins update caduceus` updates sources only; operators rerun `hermes caduceus setup` to rebuild. The setup/doctor output makes these lifecycle facts explicit.

### Error contract

```rust
#[derive(thiserror::Error, Debug)]
pub enum CaduceusError {
    Config(String),
    Io(#[from] std::io::Error),
    Json(#[from] serde_json::Error),
    Yaml(#[from] serde_yaml::Error),
    Http(#[from] reqwest::Error),
    Git { operation: &'static str, stderr: String },
    GitHubApi { status: u16, message: String },
    RateLimited { reset_at: u64, remaining: u32, limit: Option<u32> },
    TokenResolution(String),
    Worker(String),
    Worktree(String),
    Queue(String),
    StateCorrupt { path: PathBuf, message: String },
    Cancelled,
    Other(String),
}
pub type CaduceusResult<T> = Result<T, CaduceusError>;
```

No production constructor panics on malformed external data. Poisoned mutexes, invalid timestamps, invalid UTF-8 paths, and malformed queue keys return errors.

---

### Phase 0: Project scaffolding

#### Amendment 0.1: Create the Rust crate and module graph

Create `Cargo.toml`, committed `Cargo.lock`, `src/main.rs`, and modules `config`, `context`, `error`, `finalize`, `github`, `issue`, `logging`, `meta`, `migrate`, `poll`, `prompt`, `queue`, `status`, `validate`, `verify`, `worktree`, `worker`, and `worker_supervisor`. Add all dependencies listed above now so later tasks do not mutate the toolchain opportunistically. Re-export only `CaduceusError`, `CaduceusResult`, `IssueDetail`, `IssueKey`, queue enums/entries, and `WorkerResult` from `lib.rs`; modules otherwise use their canonical paths.

Acceptance: `cargo build --locked --all-targets`, `cargo fmt --check`, and `cargo clippy --locked --all-targets -- -D warnings` pass on the pinned MSRV. `main` parses the canonical CLI only; it does not print a version during a normal cron tick.

Dependencies: none.

#### Amendment 0.2: Implement and validate the Hermes adapter

Replace the historical `plugin/` scaffolding with the root-level layout in the Hermes compatibility contract. Implement root `__init__.py` registration and its explicit setup/doctor/status/cron lifecycle. Keep the adapter stdlib-only so Hermes can discover it before the Rust binary is built. All subprocess calls use argument arrays, bounded output, timeouts, and redacted errors. The bridge template remains the only harness-specific Python file; the Hermes adapter never imports it or handles daemon credentials.

Tests run against Hermes Agent 0.18.2 in an isolated `HERMES_HOME` and prove: install from repository root; plugin discovery and enablement; manifest field allowlist; skill resolution as `caduceus:caduceus`; slash and CLI command registration; missing-binary diagnostics; locked Rust build and atomic binary placement; setup idempotency; user bridge preservation and `.new` upgrade candidate; cron wrapper path/content/mode; cron zero/one/multiple-match reconciliation; no-agent execution invokes `caduceus run`; cron removal; source update followed by rebuild; plugin removal leaves user bridge/state; and registration performs no build/network/config/cron mutation. A negative fixture containing the legacy custom manifest fields must fail the contract test.

Dependencies: 0.1.

### Phase 1: Configuration, errors, and logging

#### Amendment 1.1: Parse and validate Config

Implement the canonical `Config` contract and a private deserialization layer so missing `worker_command` can be resolved after the source path is known. Compile regex patterns during validation and reject malformed allowlist IDs, invalid repository slugs, zero durations/budgets, duplicate trigger labels, and forbidden attempts to allow GitHub credential variables.

Tests: minimal plugin-derived config; every default; explicit replacement semantics for all lists; leading-tilde expansion; exact `${plugin_root}` worker-argument expansion; rejection of unknown interpolation; empty/zero rejection; invalid regex; malformed `id:`; duplicate labels; unknown YAML field rejection using `deny_unknown_fields`; standalone missing-worker error; secure directory/file modes; symlinked state path; wrong-owner path; and unsafe existing permissions.

Dependencies: 0.1.

#### Amendment 1.2: Resolve GitHub authentication

Implement the documented hierarchy without mutating global environment during parallel tests. Token tests use `serial_test` and a scoped environment guard. Shell out to `gh auth token` with captured stderr, a 10-second timeout, and no token logging. Whitespace-only output is failure.

Tests: each hierarchy level, precedence, empty values, missing `gh`, nonzero `gh`, timeout, and redacted error text.

Dependencies: 1.1.

#### Amendment 1.3: Resolve config files and environment overrides

Implement `Config::load`, `load_from(path)`, and test-only `load_with_paths(env, hermes, standalone)`. An explicitly configured missing `$CADUCEUS_CONFIG` is an error and never falls through. A present Hermes file without a `caduceus` section falls through only when a standalone file exists; otherwise it reports the missing section. Apply `CADUCEUS_DRY_RUN` after YAML parsing.

Tests: all precedence cases, explicit missing path, malformed YAML, missing section, standalone fallback, dry-run truth table, non-Unicode environment value, and paths containing spaces.

Dependencies: 1.1.

#### Amendment 1.4: Initialize structured logging safely

Return a `WorkerGuard` that keeps the nonblocking file writer alive. Create parent directories, write structured compact logs to the file, and human-readable warnings/errors to stderr. Initialization is once per process; tests use a subscriber scoped with `tracing::subscriber::with_default` rather than installing multiple globals. Secrets and environment values are never logged.

Tests: nested directory creation, flushed line after dropping guard, second initialization behavior, unwritable path, and redaction helper.

Dependencies: 1.1.

#### Amendment 1.5: Implement the unified error hierarchy

Implement the exact error contract above plus a separate `VoiceError { Forbidden { found: String }, TooLong { limit: usize } }`. Add conversions only where lossless; attach operation context to git and worker errors. Ensure `Debug` and `Display` cannot contain resolved tokens.

Tests: every automatic conversion, rate-limit display, state-corruption path, voice errors, and token redaction.

Dependencies: 0.1.

#### Amendment 1.6: Validate the worker command and runtime prerequisites

Validate nonempty command, executable lookup, readable bridge path when the second argument names the bundled bridge, installed `git`, writable state/workdir parents, and Unix process-group support. Validation runs before acquiring the daemon tick lock so configuration errors remain visible.

Tests use temporary executables and controlled `PATH`; they cover absolute and PATH commands, non-executable files, empty command, missing bridge, missing git, and unwritable directories.

Dependencies: 1.1, 1.5.

### Phase 2: GitHub client and polling

#### Amendment 2.1: Build the typed HTTP client and persistent conditional cache

Implement `github::Client::with_config(&Config) -> CaduceusResult<Client>` and a shared, mutex-protected `HttpCache` rooted at `<state_dir>/cache/http.json`. Configure a 10-second connect timeout and `http_timeout_seconds` total request timeout. Redirects are limited to three and only to the same scheme/host/port as `api_base`; authorization is never forwarded elsewhere. Responses retain their final URL so issue verification can detect a transfer. Centralize headers, status mapping, ETag validation, atomic cache writes, cached-body reuse on 304, and a streaming response-size limit of 10 MiB before full allocation. Cache keys are full URL plus relevant Accept header. Concurrent detail requests merge cache entries through one locked update; they never overwrite from independent stale snapshots.

Tests: required headers, auth header, connect/request timeout, allowed same-host redirect, cross-host redirect rejected without token forwarding, redirect loop, first 200 then a new client sending `If-None-Match`, 304 body reuse, cache corruption recovery, invalid ETag, chunked oversized body, 401/403/404/500 mapping, and no token in errors.

Dependencies: 1.2, 1.5.

#### Amendment 2.2: Discover watched repositories

If `watched_repos` is nonempty, return its validated sorted deduplicated contents without an API call. Otherwise paginate `/user/repos`, extract `full_name`, exclude archived/disabled repos, and cap at 20 pages. Repository discovery responses use the persistent HTTP cache.

Tests: configured bypass, two-page Link traversal, sorting/deduplication, empty result, archived exclusion, malformed object, page-cap error, and rate limit on page two.

Dependencies: 2.1.

#### Amendment 2.3: Poll open labeled issues with a typed schema

Define `IssueSummary { key, title, labels, ticket_type }`. Paginate the two encoded label queries for each watched repo, merge results, exclude PR objects, and exact-match trigger labels. Return summaries plus structured ambiguous-trigger diagnostics; do not mutate queue state here.

Tests use realistic GitHub issue-list fixtures and cover Unicode URL encoding, code/investigation merge, both-label rejection, server returning an unrelated-label object, PR exclusion, empty/null body tolerance, malformed number, pagination in either query, 304 reuse, and no Events API fields.

Dependencies: 2.1, 2.2, 3.0 (for the canonical `TicketType`).

#### Amendment 2.4: Handle poll cadence and rate limits

Parse `X-Poll-Interval`, `X-RateLimit-Limit`, `Remaining`, and `Reset` from every response. Persist observations immediately. At tick start, compare now with both `last_tick_started + poll_interval_seconds` and the persisted rate-limit reset; an early invocation exits 0 with a `skipped_cadence` or `rate_limited` outcome without an HTTP call. A 429 or remaining zero at any page stops pagination and becomes a clean tick outcome.

Tests: cadence skip across two process-equivalent clients, longer server poll interval, 429 mid-pagination, remaining zero on 200, missing/malformed headers, reset persistence before exit, and resumption after reset.

Dependencies: 2.1 and Task 7.2 metadata types. Implement Task 7.2's types before this task, then return here before Task 7.1.

#### Amendment 2.5: Verify the selected trigger label immediately before work

Implement `verify_trigger(client, key, ticket_type, config) -> CaduceusResult<bool>` using the full current issue response. Select the expected label by ticket type. Closed, transferred, deleted, or unlabeled issues return `false`; authentication/rate-limit/server failures return errors and do not consume a retry.

Tests: both ticket types, removed label, closed issue, 404 skip, 403 error, 429 outcome, redirect/transfer, and both-label ambiguity.

Dependencies: 2.1, 3.0.

#### Amendment 2.6: Fetch complete issue detail

Define serializable `IssueDetail`, `IssueComment`, and `IssueEvent` with explicit fields rather than tuples. Fetch issue, comments, and timeline concurrently, but cancel and return the first error. Comments use `per_page=100`, follow GitHub's default chronological pagination for at most 20 pages, then retain the most recent 100 in chronological order. Timeline follows the same cap. Preserve author login/id, body, timestamps, and label events.

Tests: complete parse, null body/user, empty data, chronological normalization, multipage comments, malformed comments, 404, rate limit in one branch of the join, and `Serialize` round-trip for context construction.

Dependencies: 2.1.

### Phase 3: Durable queue and atomic claims

#### Amendment 3.0: Implement validated queue data types

Implement the canonical `IssueKey`, `Phase`, `TicketType`, `QueueEntry`, and `QueueState` types. `IssueKey::parse` returns an error and never panics. Serialization is schema-stable, timestamps are RFC3339 UTC, and unknown fields are rejected for the current version. A future version mismatch is a `StateCorrupt`/unsupported-version error, not best-effort parsing.

Tests: display/parse round-trip, invalid owner/repo/number, lowercase enum JSON, complete state round-trip, unknown field, missing required field, and unsupported version.

Dependencies: 1.5.

#### Amendment 3.1: Implement crash-safe StateStore

```rust
pub struct StateStore { state_dir: PathBuf, state_path: PathBuf, claims_dir: PathBuf }
impl StateStore {
    pub fn open(state_dir: &Path) -> CaduceusResult<Self>;
    pub fn snapshot(&self) -> CaduceusResult<QueueState>;
    pub fn enqueue(&self, key: &IssueKey, ticket_type: TicketType, dry_run: bool) -> CaduceusResult<EnqueueOutcome>;
    pub fn acquire_next(&self, run_id: &str, pid: u32, now: DateTime<Utc>) -> CaduceusResult<Option<ClaimedEntry>>;
    pub fn set_worktree(&self, claim: &ClaimToken, path: &Path) -> CaduceusResult<()>;
    pub fn save_finalization(&self, claim: &ClaimToken, checkpoint: FinalizationCheckpoint) -> CaduceusResult<()>;
    pub fn complete(&self, claim: ClaimToken) -> CaduceusResult<()>;
    pub fn complete_investigation(&self, claim: ClaimToken) -> CaduceusResult<()>;
    pub fn retry_or_fail(&self, claim: ClaimToken, error: &str, budget: u32) -> CaduceusResult<Phase>;
    pub fn requeue_infrastructure(&self, claim: ClaimToken, error: &str, not_before: DateTime<Utc>) -> CaduceusResult<()>;
    pub fn skip(&self, claim: ClaimToken, reason: &str) -> CaduceusResult<()>;
}
```

`acquire_next` skips queued entries whose `next_attempt_at` is in the future. Each mutating method takes an exclusive `flock` on a separate `<state_dir>/state.lock`, loads and validates state, applies one transition, writes a temporary file, flushes and `sync_all`s it, renames it, syncs the directory, then releases the lock. `snapshot` takes a shared lock and never rewrites state. Errors leave the prior file intact. Completion/retry methods durably persist the new queue phase before unlinking the claim and syncing `claims_dir`; a claim-unlink failure is reported without rolling back the durable phase and is repaired idempotently by the reaper.

Tests: initialization, mutation/checkpoint round-trip, read-only snapshot mtime unchanged, truncated state error/preservation, simulated pre-rename failure, concurrent enqueues, deterministic FIFO (`queued_at`, then display key), checkpoint update under the matching claim/run only, and no lost update.

Dependencies: 3.0.

#### Amendment 3.2: Create and release atomic claims

`acquire_next` creates the SHA-256 claim path with `create_new(true)`, writes/syncs claim JSON, then marks the same entry `InProgress` while holding `state.lock`. If state persistence fails, it removes the new claim. If claim creation loses a race, it tries the next FIFO entry. `ClaimToken` contains the digest path and run ID but does not expose arbitrary deletion.

Add a separate nonblocking `DaemonLock::try_acquire(state_dir) -> CaduceusResult<Option<DaemonLock>>` held for the entire tick. Its file handle releases the lock on drop; the file itself may remain.

Tests: two threads and two subprocesses yield one claim winner, two daemon locks yield one winner, rollback after state-write failure, claim JSON durability, hostile key cannot affect paths, and completed claims are deleted.

Dependencies: 3.1.

#### Amendment 3.3: Reap stale claims and abandoned worktrees

At tick start under `DaemonLock`, scan claim JSON. A claim is stale only if its age exceeds `stale_run_hours` and its recorded process identity is absent or no longer matches. A timestamp more than five minutes in the future is corrupt rather than immortal. Malformed/future claims are quarantined to `<claims>/corrupt/` and reported; they are never silently deleted. For an `InProgress` entry, reaping returns it to `Queued` without incrementing attempts after safe worktree removal. For an entry already durably `Queued`, `Previewed`, `Done`, `Failed`, or `Skipped`, the reaper treats the claim as residue: it performs any required teardown and removes only the claim without changing phase. It returns `ReapReport { count, errors }`.

The same maintenance task removes regular, non-symlink run artifacts older than `run_retention_days` only when their run ID is absent from every active claim, live heartbeat, `InProgress` entry, and resumable finalization checkpoint. It never removes queue tombstones; `Done`, `Failed`, and `Skipped` entries remain durable to prevent accidental re-triggering. Files with unknown names are reported and left untouched.

Tests: stale dead PID, matching live PID retained, reused PID/start-identity mismatch reaped, recent dead PID retained until threshold, future timestamp quarantine, malformed quarantine, missing queue entry, residual claim for every non-in-progress phase, missing worktree, teardown failure retained for retry, claim-unlink failure after durable transition, old unreferenced run cleanup, active/checkpoint artifact retention, symlink/unknown-file rejection, and reap metadata.

Dependencies: 3.2, 4.3.

#### Amendment 3.4: Enforce retry and terminal transitions

Implement the exact three-worker-failures semantics. `retry_or_fail` increments once, records a bounded error string, applies the configured backoff, returns to `Queued` below budget, transitions to `Failed` at the budget, and always removes the claim. `requeue_infrastructure` records a bounded diagnostic and eligibility time without incrementing attempts. Rate limits use the persisted reset time; cancellation is immediately eligible after cleanup. `skip` transitions to terminal `Skipped`. `complete`/`complete_investigation` transition to `Done`; `complete_preview` transitions to `Previewed`. Re-enqueue is a no-op except that a non-dry enqueue promotes `Previewed` to `Queued`.

Add `caduceus queue reset <owner/repo#number>` as the only v0.1 recovery operation for a `Failed` or `Skipped` entry. It acquires `DaemonLock` and `state.lock`, refuses entries with an active claim, resets attempts/error/run ID, and returns the entry to `Queued` only if the issue is explicitly named. It supports `--dry-run`; there is no bulk reset. A finalization checkpoint is preserved by default. Clearing one requires `--force-finalization-reset`, reports the branch/PR that may need manual reconciliation, and never deletes a remote branch or PR automatically.

Tests: failures 1/2/3/4, worker backoff eligibility, infrastructure failure leaves attempts unchanged, rate-limit reset eligibility, cancellation immediate eligibility, zero budget rejected by config, success clears error/backoff, skip cannot be reacquired, done/failed cannot be reacquired, preview cannot be reacquired while dry, preview promotion when dry-run is disabled, claim removal on every transition, transition called with the wrong run ID, reset terminal entry, active-claim refusal, checkpoint-preserving reset, forced checkpoint reset warning, and reset dry-run.

Dependencies: 3.2.

### Phase 4: Repository and worktree lifecycle

#### Amendment 4.1: Discover and validate local clones

Implement a common `GitRunner` used by every git task. It sets `GIT_TERMINAL_PROMPT=0`, clears daemon GitHub token variables, captures bounded stdout/stderr, starts git/SSH in a process group, enforces `git_timeout_seconds`, and kills/reaps the group on timeout/cancellation. Then implement `find_main_clone(config, key) -> CaduceusResult<PathBuf>` at `<workdir_base>/<owner>/<repo>`. Require a git worktree, a clean main checkout, and an `origin` whose normalized owner/repo matches the issue slug. For the public API base, remote host must be `github.com`; for a GitHub Enterprise API base it must equal that URL's host. SSH host aliases are intentionally rejected in v0.1 because their destination cannot be authenticated from the remote string. Determine the default base branch from `refs/remotes/origin/HEAD`, falling back to the repository's current branch only with a warning. Return `RepositoryInfo { path, base_branch, remote_url }`.

Tests: SSH/HTTPS origin normalization, official/enterprise host validation, SSH alias/host mismatch rejection, missing repo, non-git directory, slug mismatch, detached HEAD without origin HEAD, dirty main checkout, paths containing spaces, prompt suppression, timeout with SSH-like grandchild, cancellation, and stderr redaction/truncation.

Dependencies: 1.6, 3.0.

#### Amendment 4.2: Create a daemon-owned worktree and branch

Implement `create(config, repo, key, run_id) -> CaduceusResult<Worktree>` using `git fetch --prune origin <base>` followed by `git worktree add -b <branch> <path> origin/<base>`. `Worktree` records the initial base OID. Branch is `automation/issue-<number>-<lowercase-run-id>` and is validated with `git check-ref-format --branch`. Worktree path is `<repo>/.worktrees/<run_id>` rather than the slash-containing branch. Pre-existing local/remote branch or path is reconciled only when it belongs to the same run ID; otherwise return a collision error.

Tests use a local bare origin and cover successful creation, default base, branch/path separation, fetch failure, collision, invalid run ID, and parent checkout unchanged.

Dependencies: 4.1.

#### Amendment 4.3: Tear down safely

Implement `remove(worktree)`: `git worktree remove --force <path>`, then `git worktree prune`, then delete the local branch only when no finalization checkpoint needs it and it was not pushed. Dry-run and pre-commit worker-failure branches are removed; committed/resumable or pushed branches are retained. Missing paths are idempotent. Never use raw recursive deletion until git has removed its registration; a final filesystem fallback must verify the path is beneath the expected `.worktrees` directory.

Tests: success/failure/dry-run teardown, already missing path, nested filesystem contents, registered metadata removed, pushed branch retained, and path-escape rejection.

Dependencies: 4.2.

#### Amendment 4.4: Generate the canonical prompt file

Implement a pure `build_prompt(issue, ticket_type, context_json, branch_name) -> String` and atomic `write_prompt(worktree, text)`. The prompt states the exact output schema, daemon-owned branch, forbidden modification of `.git` and control files, prohibition on committing/pushing/checking out or renaming branches, code versus investigation behavior, and that daemon GitHub access is unavailable. Fence issue body and context safely so adversarial Markdown cannot terminate structural sections. The encoded prompt is limited to 2 MiB; exceeding the bound is a non-retryable input/configuration diagnostic rather than a partial write.

Tests: title/body/labels/repo/number/context/branch, exact investigation selection, Markdown fence injection, empty body, Unicode, maximum input size, and file write failure.

Dependencies: 2.6, 5.6.

#### Amendment 4.5: Implement safe worktree GC

`caduceus worktree-gc [--older-than-days 7] [--dry-run]` enumerates registered worktrees using `git worktree list --porcelain` across validated repositories. It excludes any path referenced by a claim or fresh heartbeat, removes only paths beneath that repo's `.worktrees`, and uses Task 4.3. Unregistered directories are reported but removed only when their canonical path is safe and they are old, inactive, and not symlinks.

Tests: multiple repos, nested branch names irrelevant, old active worktree retained, symlink rejected, unregistered orphan, dry-run no mutation, and git metadata cleanup.

Dependencies: 3.1, 4.3, 5.1. Heartbeat parsing is a shared helper in `worker`; status and GC call the same implementation.

### Phase 5: Worker execution and context

#### Amendment 5.0: Define finalization interfaces without runtime stubs

Define only types and traits needed to compile earlier tasks; do not add `unimplemented!()` production functions. `FinalizeContext` includes client, config, repository info, issue, claim/run data, worktree, and result. `FinalizeOutput` records action, PR URL, and idempotency observations. Phase 6 supplies concrete functions before orchestration is enabled behind a temporary `compile_orchestrator` feature.

Acceptance: default builds contain no reachable `unimplemented!()`, `todo!()`, or `panic!()` outside tests.

Dependencies: 2.6, 3.2, 4.2.

#### Amendment 5.1: Spawn and supervise the entire worker process tree

Implement:

```rust
pub async fn spawn(
    cfg: &Config,
    issue: &IssueDetail,
    worktree: &Worktree,
    run_id: &str,
    context_json: &str,
    cancellation: CancellationToken,
) -> CaduceusResult<WorkerResult>;
```

The public daemon never spawns the bridge directly. It spawns the same binary with hidden `__worker-supervisor` mode and dedicated control/status pipes. The supervisor stays outside the worker session and forks the worker behind an exec-gate pipe. The worker calls `setsid` but cannot exec the bridge until the supervisor sends `READY(pgid)`, the daemon records that PGID and replies `ACK`, and the supervisor opens the gate. If either side dies before ACK, gate EOF makes the pre-exec child exit without running the harness. After ACK, unexpected supervisor exit makes the daemon kill the recorded session; daemon death closes the control pipe and makes the live supervisor kill it.

On Linux the supervisor sets `PR_SET_CHILD_SUBREAPER` before spawning. Cleanup enumerates its descendant PIDs from `/proc`, signals both the original negative PGID and every descendant (including children that created another process group/session), waits at most two seconds, repeats discovery, sends KILL, and reaps until no descendants remain. On other Unix platforms it uses the worker process group and documents that deliberately detached descendants require container-level isolation. EOF on the control pipe means the daemon died and triggers this sequence. Normal timeout/cancellation uses an explicit control message. When the direct bridge exits, the supervisor also cleans remaining descendants before returning its status.

Create `<state_dir>/runs/<run_id>.log` and heartbeat before supervisor spawn. Forward stdout/stderr through one bounded async channel to a single transcript writer so chunks cannot race file offsets. Retain at most `transcript_max_bytes`, append one truncation marker, and continue draining/discarding until the supervisor closes both streams. Await supervisor, both readers, and writer; remove heartbeat only afterward. Spawn/protocol failures close the control pipe, trigger session cleanup, and remove heartbeat. The supervisor protocol is versioned and length-bounded, uses inherited file descriptors rather than filesystem sockets, and receives only the cleared worker environment—not daemon credentials.

Tests: exit 0, nonzero, timeout, SIGINT cancellation, transcript content, concurrent stdout/stderr, truncation marker, disk-write failure, missing command, supervisor protocol corruption, direct-child exit with a live grandchild, a grandchild that calls `setsid` on Linux, heartbeat visible while live and removed afterward, daemon SIGKILL/control-pipe EOF, and supervisor crash. PID assertions use bounded polling and captured diagnostics rather than sleeps alone.

Dependencies: 5.2, 5.3. `CancellationToken` is part of this task's public interface; Task 7.4 only supplies signal-driven cancellation.

#### Amendment 5.2: Construct a deny-by-default worker environment

Implement `sanitized_env(args) -> CaduceusResult<BTreeMap<OsString, OsString>>` from an injected parent environment for testability. `spawn` must call `env_clear()` before `envs`. Preserve exact default variables and configured prefix patterns, but hard-deny `GITHUB_TOKEN`, `GH_TOKEN`, `CADUCEUS_GITHUB_TOKEN`, `AUTO_ISSUE_GITHUB_TOKEN`, variables containing `GITHUB` plus `TOKEN`, and any daemon-internal secret. Emit labels as JSON and all paths as absolute UTF-8 or return an error.

Tests execute a real child that dumps its environment; cover all contract variables, empty labels, labels containing commas, denied credentials despite allowlist, approved provider keys, unrelated AWS secret removed, invalid path, and values absent from logs.

Dependencies: 1.1, 2.6, 4.2.

#### Amendment 5.3: Parse and validate worker results

Define the canonical schema with `#[serde(deny_unknown_fields)]` and `artifacts: BTreeMap<String, Value>`. Open with `O_NOFOLLOW`, verify the opened descriptor is a regular file, and read with a 1 MiB limit before allocating the full file. Validate status/lengths and wrap all file/schema failures as contextual `CaduceusError::Worker`. Code tickets require meaningful repository changes later in finalize; investigation does not.

Tests: valid minimal, nested artifact values, malformed artifacts container, unknown field, missing/empty/oversized fields, title newline/control/NUL, multiline commit message, artifact key/count limits, wrong status, invalid UTF-8, oversized file, symlink, missing file, and error variant consistency.

Dependencies: 1.5.

#### Amendment 5.4: Render artifacts and public PR text safely

Implement deterministic artifact rendering sorted by key. Values render in fenced JSON with dynamically chosen fence length, total rendered output is capped, and control characters are escaped. `build_pr_body` combines summary, artifact section, issue-closing reference, and an idempotency marker comment. Validate title and body through the public-voice rule before returning.

Tests: empty/nonempty/nested artifacts, Markdown fence injection, stable order, size cap, forbidden string in summary/artifact/title, and marker presence.

Dependencies: 5.3, 6.6.

#### Amendment 5.5: Implement dry-run as a first-class outcome

Dry-run executes through result and change validation, writes an atomic report under `state_dir/runs`, skips every git/GitHub mutation including commit, then completes the queue entry as `Previewed` and tears down. The report includes proposed branch, commit, PR title/body or investigation comment, changed-file list, transcript path, and validation warnings. When dry-run is later disabled, normal polling promotes a still-labeled preview back to `Queued` exactly once.

Tests assert no commit, remote ref, HTTP mutation, label change, or worktree remains; report survives teardown; queue becomes `Previewed`; disabling dry-run promotes it; and worker failure still consumes retry budget.

Dependencies: 5.3, 5.4, 6.6.

#### Amendment 5.6: Build stable context JSON

Emit schema version, issue timeline, filtered `issue_comments`, and explicit `trusted_comments`. Parse allowlist entries during config validation. Invalid ignore regexes are configuration errors, never silently dropped. A comment is trusted by exact login or numeric ID; ignored authors are absent from both arrays. Cap each comment body at 64 KiB and the encoded context at 1 MiB. Remove oldest untrusted comments first, then oldest trusted comments only if necessary, and emit counts/byte truncation metadata; timeline events are similarly bounded before failing.

Tests: empty schema snapshot, login/id trust, rename-resistant ID, ignored bot, invalid regex rejected in config, timeline serialization, stable chronological order, exact per-body/total boundaries, trusted-last truncation order/metadata, irreducibly oversized timeline, Unicode, and JSON round-trip.

Dependencies: 1.1, 2.6.

### Phase 6: Idempotent finalization

#### Amendment 6.1: Inspect changes and commit code results

First require `HEAD` still equals the worktree's recorded initial OID; a worker-created commit, checkout, merge, rebase, or detached HEAD is a worker-contract failure. Then use `git status --porcelain=v2 -z` to collect changed paths. Exclude control files explicitly; reject symlinks escaping the worktree and changes under `.git`. A code success with no remaining changes becomes a worker failure. Stage with `git add --all -- <validated paths>` and commit using the worker message with configured daemon identity. Atomically copy the validated result to the runs directory, commit, then persist a `Committed` checkpoint containing the OID and daemon branch before teardown. A retry with an existing committed checkpoint skips worker and commit.

Immediately before this first finalization mutation, re-fetch the issue and require it is still open with exactly the expected trigger-label state. A user withdrawal transitions the claim to `Skipped`, writes a diagnostic/report preserving the worker summary locally, removes the unpushed branch/worktree, and performs no commit, push, or GitHub mutation.

Tests: tracked/untracked/deleted/renamed files, only control files, no changes, worker-created commit/checkout/detached HEAD, path with newline, escaping symlink, commit identity, commit message length, and parent checkout untouched.

Dependencies: 4.2, 5.3.

#### Amendment 6.2: Push idempotently through git

Push the checkpoint's local branch as `refs/heads/<daemon branch>` using the operator's configured git credential mechanism. Never place the PAT in arguments, URLs, or environment. Query the remote first: absent ref is created; identical ref is success; an ancestor ref is fast-forwarded; divergent ref is a terminal collision. Persist `Pushed` immediately after success. Capture bounded stderr with secrets redacted.

Tests use local bare origin for absent/identical/fast-forward/divergent refs and a fake git credential helper to assert no API PAT injection. A hanging remote/credential helper must hit `git_timeout_seconds`, kill descendants, and leave the claim recoverable.

Dependencies: 6.1.

#### Amendment 6.3: Find or create the pull request

Load the checkpoint's secure worker result and revalidate issue/trigger state, then query `GET /repos/{slug}/pulls?state=open&head={owner}:{branch}&base={base}` before POST. Reuse exactly one matching PR; error on multiple matches. Otherwise POST title/body/head/base and require a 201 with number and URL. A retry after a lost response re-queries before posting. Persist PR number/URL and `PrCreated` immediately. Validate all public text first. Withdrawal after push but before PR creation leaves the remote branch/checkpoint recorded, transitions to `Skipped`, and reports the branch for manual cleanup; it never creates the PR anyway.

Tests: create, reuse, multiple-match error, malformed response, 422 followed by successful re-query, 429, forbidden text prevents request, and exact base/head.

Dependencies: 2.1, 5.4, 6.2.

#### Amendment 6.4: Post completion and close idempotently

Use a hidden marker `<!-- automation-run:<checkpoint.run_id> -->` to detect an existing completion comment. Before posting/closing, fetch issue state and trigger labels again. If the PR already exists but the trigger was withdrawn, leave both PR and issue open, persist the checkpoint, transition to `Skipped`, and emit a diagnostic linking the PR; do not post completion or close. Otherwise post only if absent and persist `Commented`; already-closed issue is success. If comment succeeds and close fails, retry resumes from the checkpoint and detects the comment without a worker run. Do not claim HTTP ordering without verifying mock expectations.

Tests: fresh, comment exists, already closed, partial failure then retry, voice rejection, 404, and rate limit.

Dependencies: 6.3, 6.6.

#### Amendment 6.5: Finalize failures and investigations

Worker/process failures update local state first. After revalidating that the issue remains open/triggered, a best-effort generic failure comment may name the run ID but never claims a local transcript is a public link; withdrawal suppresses the comment, and comment failure does not hide the original error. Investigation success copies the result to the runs directory and persists `InvestigationReady` before posting findings. Findings combine summary with the same bounded, injection-safe artifact renderer used for PR bodies. Revalidate the issue/label immediately before posting and again before label removal; withdrawal skips the remaining mutations. Otherwise post/reuse the marker comment, persist `InvestigationCommented`, remove only the configured investigation label, leave the issue open, and perform no git mutation. A retry resumes the checkpoint without invoking the worker. Both paths are idempotent and voice-checked.

Tests: failure comment, forbidden original error not leaked, comment API failure preserves worker error, investigation comment/label removal, retry marker reuse, issue remains open, and no push/PR calls.

Dependencies: 5.3, 6.6.

#### Amendment 6.6: Enforce the public-voice rule

Implement `validate_public_text(text, cfg, limit) -> Result<(), VoiceError>` using case-insensitive Unicode substring matching and byte-length limits. Call it in the only HTTP helpers capable of posting comments, PR titles, or PR bodies so callers cannot bypass it. Log the matched configured term only if the term itself is not sensitive; never log rejected text.

Tests: defaults, case variants, substring behavior, explicit replacement, empty forbidden entry rejected by config, PR body/artifact enforcement, max length, and proof that a rejected request never reaches Wiremock.

Dependencies: 1.1, 1.5.

### Phase 7: Orchestration, metadata, status, and system tests

#### Amendment 7.0: Define orchestration-owned types and dependency injection

Define `Services { clock, github, git, process }` traits/adapters used only where deterministic testing requires them; production adapters remain thin. Define `TickOutcome` (`Processed`, `Idle304`, `IdleEmpty`, `SkippedConcurrent`, `SkippedCadence`, `RateLimited`, `Cancelled`, `Failed`), `FailureClass` (`Worker`, `Infrastructure`, `RateLimit { reset_at }`, `Cancellation`), and one exhaustive `classify_error` match. Schema/result/content/no-code-change/worker-exit/timeout failures are `Worker`; HTTP transport/server, git transport, filesystem, and teardown failures are `Infrastructure`; typed rate limits and cancellation retain their own class. New `CaduceusError` variants make this match fail to compile until classified.

Define `ActiveRunGuard`, which owns claim, optional worktree, supervisor handle/worker PGID, and cancellation state. Its async `finish_*` methods perform explicit transitions; `Drop` logs an invariant violation but synchronous drop is not relied upon for cleanup.

There is one canonical worker signature (Task 5.1), one canonical worktree type (Task 4.2), and one canonical finalization context (Task 5.0). Delete any earlier temporary signatures when enabling orchestration.

Acceptance: a repository-wide check finds no `unimplemented!`, `todo!`, placeholder ellipses, duplicate public function declarations, or `expect`/`unwrap` in production modules handling external input.

Dependencies: 3.4, 4.3, 5.1, 6.5.

#### Amendment 7.1: Implement the single canonical tick

The exact order is:

1. Load/validate config and initialize logging in `run`; injected config in `run_with_config` is still validated.
2. Try `DaemonLock`; if unavailable, persist nothing and return `SkippedConcurrent`/exit 0.
3. Load metadata, enforce rate-limit and cadence gates, then atomically persist `last_tick_started` and `running` outcome.
4. Reap stale claims/worktrees, apply safe run-artifact retention, and persist the report.
5. Build the GitHub client, discover repositories, poll typed open issues, and enqueue summaries.
6. Persist rate-limit observations after every response through the client observer.
7. Generate a fresh candidate run ID and call `acquire_next`. The store uses the checkpoint's existing run ID when present and the candidate otherwise. If no entry is eligible, finish as `Idle304` only when every poll response was 304; otherwise `IdleEmpty` and report the earliest backed-off eligibility time when relevant.
8. If the acquired entry contains a finalization checkpoint, jump directly to the matching Phase 6 resume stage without verification/worker/worktree recreation unless that stage specifically needs the retained local branch. Otherwise continue the fresh run using the claim's returned run ID.
9. Verify the ticket-type-specific label. False calls `store.skip`; transport/rate-limit error releases through a non-attempting requeue operation.
10. Fetch `IssueDetail`; build context; discover repo; create worktree/branch; persist worktree in claim; write prompt.
11. Spawn the worker with cancellation. Classify every error as `Worker`, `Infrastructure`, `RateLimit`, or `Cancellation`. Worker/result/content-validation failures use `retry_or_fail`; GitHub/git/I/O failures use `requeue_infrastructure`; rate limits use their reset time; cancellation does not increment attempts. Teardown precedes the queue transition, then worker-attributable failures may send a best-effort notification.
12. On success, revalidate issue/trigger state, then perform dry-run, investigation, or code finalization. Finalization revalidates again at each irreversible GitHub boundary. Teardown always runs. Only after required side effects succeed does the store complete the claim; user withdrawal follows the explicit `Skipped` rules in Phase 6.
13. Persist finish time/outcome/error and return exit 0 for successful, idle, concurrent, cadence, and rate-limit outcomes; configuration/state/invariant failures return exit 1.

Every step after claim acquisition is inside one explicit cleanup scope. If primary work and teardown both fail, return a compound error preserving both messages and leave enough claim data for reaping.

Public signatures:

```rust
pub fn run() -> CaduceusResult<u8>;
pub async fn run_with_config(cfg: Config, cancellation: CancellationToken) -> CaduceusResult<TickOutcome>;
pub async fn tick(cfg: Config, services: Services, cancellation: CancellationToken) -> CaduceusResult<TickOutcome>;
```

Tests: concurrent lock skip, cadence skip, empty/304 distinction, code happy path, investigation path, label removed before work, label removed after worker, label removed after push, label removed after PR, detail-fetch error without retry consumption, worker failure retry/backoff, timeout, finalize validation versus transport classification, teardown failure, rate limit at every fetch/finalize stage, and metadata finish on all paths.

Dependencies: all Tasks 2.x–6.x and 7.2.

#### Amendment 7.2: Persist complete daemon metadata

Define versioned `StateMeta` and a shared `MetaStore`. `MetaStore::update` serializes read-modify-write operations through a mutex plus the crash-safe file writer. Concurrent HTTP responses merge rate-limit observations; an observation with an older timestamp cannot overwrite a newer one. `StateMeta` is:

```rust
pub struct StateMeta {
    pub version: u32,
    pub last_tick_started: Option<DateTime<Utc>>,
    pub last_tick_finished: Option<DateTime<Utc>>,
    pub last_outcome: Option<TickOutcome>,
    pub last_http_status: Option<u16>,
    pub next_allowed_poll_at: Option<DateTime<Utc>>,
    pub last_reap_at: Option<DateTime<Utc>>,
    pub last_reaped_count: u32,
    pub rate_limit: Option<RateLimitObservation>,
    pub last_error: Option<String>,
    pub recent_diagnostics: Vec<DaemonDiagnostic>, // newest 20, bounded fields
}
```

`DaemonDiagnostic` contains timestamp, stable code, optional issue key, and a bounded human message. Poll ambiguity, cache recovery, reaper quarantine, and infrastructure failures append diagnostics; duplicate `(code, issue_key)` entries within one hour coalesce instead of growing the file.

Implement strict load and crash-safe save in `meta.rs`. Corrupt metadata is copied to a timestamped diagnostic file and a `<state_dir>/state_meta.corrupt` marker is written, but the active file is preserved and the tick fails closed. Later ticks refuse GitHub calls while the marker exists; only the documented recovery command may clear it after validation. Queue state is untouched. To break the Phase 2 dependency, `RateLimitObserver` is defined here before Task 2.4 implementation begins, or Tasks 2.4 and 7.2 are assigned together.

Tests: full/minimal round-trip, atomic-write failure, corrupt-file preservation/marker/fail-closed next tick, unsupported version, concurrent observer updates without lost fields, stale observation cannot replace newer data, diagnostic cap/coalescing, and timestamp serialization.

Dependencies: 1.5. Prerequisite for 2.4 and 7.1.

#### Amendment 7.3: Implement status and heartbeat inspection

Rust worker supervision writes heartbeat JSON `{version,run_id,pid,started_at,updated_at,issue_key,transcript_path}` atomically every 30 seconds. `live_workers` accepts only regular non-symlink `.heartbeat` files updated within 90 seconds, parses the run ID directly from `file_stem`, validates paths, and uses saturating elapsed-time arithmetic.

`build_report(config) -> CaduceusResult<StatusReport>` uses `StateStore::snapshot`, strict metadata load, phase counts including `Previewed`, FIFO eligible queued head, earliest future eligibility when all queued entries are backed off, at most 10 recent failed/skipped queue errors plus daemon diagnostics (including ambiguous trigger labels), and live workers. Human output has golden fixtures matching README; JSON has a schema-version field. `status` loads normal config so custom state directories work.

Tests: exact README idle output, running output with transcript, JSON snapshot, all phase counts, deterministic head, missing state diagnostic, corrupt state diagnostic, fresh/stale/future/malformed/symlink heartbeat, and custom config path.

Dependencies: 3.1, 7.2.

#### Amendment 7.4: Handle SIGINT and SIGTERM through cancellation

Install Unix signal listeners before calling `tick` and cancel a shared `CancellationToken`. Worker supervision selects on it, commands the supervisor cleanup sequence, awaits drains, and returns `Cancelled`; `ActiveRunGuard` tears down and requeues without consuming retry budget because operator shutdown is not a worker failure. A second signal requests the supervisor's immediate KILL phase but still avoids deleting state files directly. Idle cancellation exits 0.

Tests invoke the built `CARGO_BIN_EXE_caduceus`, not the test binary. Cover idle SIGINT, worker SIGTERM, grandchild death, claim removal/requeue, worktree cleanup, transcript flush, second signal, and no retry increment.

Dependencies: 5.1, 7.1.

#### Amendment 7.5: Full-system integration suite

Build reusable fixtures: Wiremock GitHub server with request expectations, local main repo plus bare origin and `origin/HEAD`, executable worker scripts, isolated config/state, and the real binary where CLI behavior matters.

Required scenarios:

1. Code success: repository discovery, paginated issue list, verify, detail/comments/timeline, prompt/env, source edit, result, commit, remote branch, one PR, one marker comment, close, done state, removed worktree/claim, persisted transcript/metadata.
2. Investigation success: findings comment and label removal, no commit/push/PR/close.
3. Second idle invocation: persisted ETags cause 304 and status reports `Idle304`.
4. Partial PR response failure followed by retry: one remote branch and one PR.
5. Timeout with grandchild: both processes die, transcript persists, issue requeues/then fails at exact budget.
6. Two concurrent binaries: only one makes HTTP calls and runs a worker.
7. Rate limit on page two: observation persists, exit 0, next pre-reset tick makes zero calls.
8. Corrupt `state.json`: exit 1, original bytes preserved, no worker/API mutation.
9. Dry-run: worker and validation occur, report persists, no git/GitHub mutation, worktree removed.
10. SIGTERM: process tree dies and claim returns to queued without attempt increment.

Every Wiremock mutation has an exact expected call count. Tests assert both desired effects and forbidden effects.

Dependencies: 7.1–7.4.

### Phase 8: Plugin and documentation contract

#### Amendment 8.1: Finalize the reference bridge and public docs

The canonical bridge reads required environment values with clear errors, reads labels from `CADUCEUS_ISSUE_LABELS_JSON`, verifies the prompt, invokes the configured harness, and returns its code. It does not write heartbeats or state. `invoke_harness` is the only user-editable function. The plugin manager's preservation behavior must be verified by an actual Hermes plugin test or described as conditional rather than promised.

Python tests: success/nonzero propagation, missing prompt/env, malformed label JSON, arguments containing spaces/Unicode, signal propagation, and no heartbeat/state writes.

Update README, plugin skill, Hermes adapter help, manifest, setup/doctor output, and cron-wrapper template from the canonical contracts. Remove claims that Hermes reads `commands/*.md`, cron-profile YAML, manifest config defaults, binary declarations, or lifecycle hooks. Add a cross-document test that extracts config keys and worker environment names and compares them with Rust fixtures, plus a Hermes contract test pinned to v0.18.2. Document the explicit `hermes plugins install` → enable → `hermes caduceus setup` → `hermes caduceus cron-install` flow; update/rebuild and cron-remove/uninstall order; plugin skill's opt-in namespace; gateway/cron-provider requirement; standalone requirement for explicit `worker_command`; required `<workdir_base>/<owner>/<repo>` pre-clones and matching origin/default branch; minimum fine-grained PAT permissions (Metadata read, Contents/Issues/Pull requests read-write); separate git authentication expectations; retry meaning; investigation behavior; dry-run behavior; transcript locality; and state recovery procedure.

Dependencies: 7.5.

### Phase 9: Migration and release readiness

#### Amendment 9.1: Write migration and recovery procedures

Create `MIGRATION.md` covering legacy cron disablement, schema import through a supported `caduceus migrate-state` command, dry-run rollout, rollback, corrupt-state and corrupt-metadata-marker recovery, failed-entry inspection/reset policy, credential-helper setup, and preservation of state on uninstall. Recovery validates a supplied repaired/generated file under the daemon lock, atomically installs it, archives the corrupt original, and only then clears a corruption marker. Never instruct users to edit `state.json`, metadata, or claims in place.

Tests: migration fixtures for empty, queued, failed, malformed, duplicate, and already-current state; migration is atomic and leaves a backup.

Dependencies: 7.5.

#### Amendment 9.2: Execute the release gate and cutover checklist

The v0.1 release cannot ship until all gates pass:

- `cargo fmt --check`
- `cargo clippy --locked --all-targets -- -D warnings`
- `cargo test --locked --all-targets` including subprocess/signal tests on Linux
- the same build/test suite under Rust 1.75
- `pytest tests/bridge_test.py`
- root-plugin install/enable/setup/update/rebuild/cron/remove lifecycle test against Hermes Agent 0.18.2
- standalone install smoke test
- no `todo!`, `unimplemented!`, placeholder ellipses, ignored tests, or network-dependent tests
- README/config/env/schema cross-document contract test
- secret scan of transcripts, logs, errors, git remotes, and child environment fixtures
- manual dry-run against a disposable repository

Cutover runs Caduceus alone against one disposable repo first. Running it beside a legacy processor against the same labels is forbidden because both can act on the same issue. Expand repository scope only after status, retry, rate-limit, and rollback checks pass.

Dependencies: all prior tasks.

### Task file and test ownership map

Every task owns the listed production and primary test files. Shared fixture edits are allowed, but changing another task's public contract requires updating that task section first.

| Task | Production files | Primary tests |
|---|---|---|
| 0.1 | `Cargo.toml`, `Cargo.lock`, `src/main.rs`, `src/lib.rs`, all module stubs | compile/fmt/clippy gates |
| 0.2 | root `plugin.yaml`, `__init__.py`, `skills/caduceus/SKILL.md`, `plugin-assets/*` | `tests/hermes_plugin_test.py` |
| 1.1 | `src/config.rs` | `tests/config_test.rs` |
| 1.2 | `src/config.rs` | `tests/token_test.rs` |
| 1.3 | `src/config.rs` | `tests/config_resolution_test.rs` |
| 1.4 | `src/logging.rs` | `tests/logging_test.rs` |
| 1.5 | `src/error.rs` | `tests/error_test.rs` |
| 1.6 | `src/validate.rs` | `tests/validate_test.rs` |
| 2.1 | `src/github.rs` | `tests/github_client_test.rs` |
| 2.2 | `src/poll.rs` | `tests/repository_poll_test.rs` |
| 2.3 | `src/poll.rs` | `tests/issue_poll_test.rs` |
| 2.4 | `src/github.rs`, `src/meta.rs` | `tests/rate_limit_test.rs`, `tests/cadence_test.rs` |
| 2.5 | `src/verify.rs` | `tests/verify_test.rs` |
| 2.6 | `src/issue.rs` | `tests/issue_detail_test.rs` |
| 3.0 | `src/queue.rs` | `tests/queue_model_test.rs` |
| 3.1 | `src/queue.rs` | `tests/state_store_test.rs` |
| 3.2 | `src/queue.rs` | `tests/claim_test.rs`, `tests/daemon_lock_test.rs` |
| 3.3 | `src/queue.rs` | `tests/reaper_test.rs` |
| 3.4 | `src/queue.rs`, `src/main.rs` | `tests/retry_test.rs`, `tests/queue_reset_cli_test.rs` |
| 4.1 | `src/worktree.rs` | `tests/repository_discovery_test.rs` |
| 4.2 | `src/worktree.rs` | `tests/worktree_create_test.rs` |
| 4.3 | `src/worktree.rs` | `tests/worktree_remove_test.rs` |
| 4.4 | `src/prompt.rs` | `tests/prompt_test.rs` |
| 4.5 | `src/worktree.rs`, `src/main.rs` | `tests/worktree_gc_test.rs` |
| 5.0 | `src/finalize.rs` types only | compile gate |
| 5.1 | `src/worker.rs`, `src/worker_supervisor.rs`, `src/main.rs` internal dispatch | `tests/worker_process_test.rs`, `tests/worker_parent_death_test.rs` |
| 5.2 | `src/worker.rs` | `tests/worker_env_test.rs` |
| 5.3 | `src/worker.rs` | `tests/worker_result_test.rs` |
| 5.4 | `src/finalize.rs` | `tests/pr_body_test.rs` |
| 5.5 | `src/finalize.rs` | `tests/dry_run_test.rs` |
| 5.6 | `src/context.rs` | `tests/context_test.rs` |
| 6.1 | `src/finalize.rs` | `tests/commit_test.rs` |
| 6.2 | `src/finalize.rs` | `tests/push_test.rs` |
| 6.3 | `src/finalize.rs`, `src/github.rs` | `tests/pr_test.rs` |
| 6.4 | `src/finalize.rs`, `src/github.rs` | `tests/issue_close_test.rs` |
| 6.5 | `src/finalize.rs`, `src/github.rs` | `tests/failure_investigation_test.rs` |
| 6.6 | `src/finalize.rs`, `src/github.rs` | `tests/voice_rule_test.rs` |
| 7.0 | `src/lib.rs` orchestration types | compile/placeholder audit |
| 7.1 | `src/lib.rs`, `src/main.rs` | `tests/tick_test.rs` |
| 7.2 | `src/meta.rs` | `tests/meta_test.rs` |
| 7.3 | `src/status.rs`, `src/main.rs` | `tests/status_test.rs`, golden fixtures |
| 7.4 | `src/main.rs`, `src/worker.rs` | `tests/signal_test.rs` |
| 7.5 | fixture helpers only | `tests/integration_test.rs` |
| 8.1 | root Hermes assets, `README.md` | `tests/bridge_test.py`, `tests/docs_contract_test.rs`, `tests/hermes_plugin_test.py` |
| 9.1 | `MIGRATION.md`, `src/migrate.rs`, `src/main.rs` | `tests/migration_test.rs` |
| 9.2 | CI/release configuration | full release gate |

#### Required task execution protocol

1. Read this document's canonical contracts plus the task and every declared prerequisite.
2. Write the named failing tests first. A RED run must fail for the missing behavior, not because the test does not compile for an unrelated reason.
3. Implement only the task contract, then run its primary tests and `cargo test --locked --all-targets`.
4. Run fmt/clippy with warnings denied. Subprocess tests must report PIDs, exit states, and retained logs on failure.
5. Remove temporary stubs and generated artifacts. Do not commit ignored/disabled tests.
6. Handoff with exact signatures, state/schema changes, commands, and test results.

### Dependency graph and task assignment rules

Critical path:

`0.1 -> 1.1/1.5 -> 1.2/1.3/1.6 -> 2.1 -> 7.2(types) -> 2.2/2.6 -> 3.0 -> 2.3/2.4/2.5 -> 3.1 -> 3.2 -> 3.4 -> 4.1 -> 4.2/4.3 -> 5.2/5.3/5.6 -> 4.4 -> 5.1 -> 6.6 -> 5.4 -> 6.1 -> 6.2 -> 6.3 -> 6.4/6.5 -> 7.0 -> 7.1 -> 7.3/7.4 -> 4.5 -> 7.5 -> 8.1 -> 9.1 -> 9.2`

Special ordering:

- Implement 7.2's metadata types before 2.4; implement 2.4 behavior before 7.1.
- Implement 3.0's shared identity/ticket types before 2.3 and 2.5.
- Implement 4.3 before 3.3 so the reaper never uses raw deletion.
- Implement 5.6 before 4.4, and 5.1's heartbeat reader before 4.5.
- Implement 6.6 before 5.4 and all GitHub mutation helpers.
- Implement all Phase 6 concrete functions before enabling the Phase 7 orchestrator.
- Task 7.5 is not divisible by deleting assertions; each scenario is a required release property.

Each task handoff must include: files changed, exact public signatures used, tests added, commands run, and any contract change. A task is not complete if tests pass only in isolation but `cargo test --all-targets` fails. Process-global environment/subscriber/signal tests must be serialized or moved to subprocesses.

### Resolved decisions

1. **JSON state on one local host.** SQLite and multi-host state remain deferred, but JSON writes are atomic and recoverable.
2. **PAT/API token authentication for v0.1.** GitHub App authentication is deferred. Git pushes use normal git credential helpers or SSH, not API-token injection.
3. **Never auto-merge.** Code tickets open PRs for human review.
4. **Public-voice enforcement is mandatory.** It covers every public string, including worker-derived PR content.
5. **Hermes-primary configuration with standalone fallback.** Explicit plugin setup seeds a user-owned bridge under `HERMES_HOME`; standalone users configure `worker_command` explicitly.
6. **Rust/Python boundary remains fixed.** Python translates harness invocation only; Rust owns all durable/runtime state.
7. **One-shot cron model remains fixed.** Cross-invocation ETags, cadence metadata, and a full-tick lock make it safe.
8. **Daemon-owned branches.** Worker-selected refs are removed from the stable bridge contract.
9. **Investigation is a comment workflow, not a PR workflow.** It posts findings, removes its trigger label, and leaves the issue open.

### Explicit v0.2+ deferrals

- SQLite/PostgreSQL state and multi-host high availability.
- GitHub App authentication.
- Auto-merge and CI/review gating.
- Native Hermes dashboard widgets. The shipped `/caduceus-status` chat command is v0.1 scope and is not deferred.
- Parallel workers. v0.1 intentionally processes one issue per host-wide tick.
- Automatic reset of terminal failed entries. v0.1 recovery tooling is explicit and auditable.

No deferred item is required to uphold a v0.1 promise. Process-group termination, durable ETags, crash-safe JSON, idempotent finalization, and chat status are explicitly not deferred.

### Risk register

| Risk | Required v0.1 control | Verification |
|---|---|---|
| GitHub schema/API drift | Typed issue-list schema and pinned API version | Realistic fixtures + header tests |
| Rate-limit storm | Persist reset before exit; preflight gate | Two-tick integration test |
| Concurrent cron ticks | Whole-tick nonblocking flock | Two-binary integration test |
| State corruption | temp+fsync+rename; preserve/quarantine corruption | fault-injection tests |
| Worker descendants survive | Rust supervisor + control-pipe EOF + worker-session kill | grandchild and daemon-SIGKILL tests |
| Daemon credential reaches worker/log/git | env clear/allowlist, redaction, credential helper | real child env + secret scan |
| Hostile same-user worker reads host files | Explicitly not an OS sandbox; document separate-user/container deployment | docs/install smoke test |
| Retry loop | exact total-attempt budget and claim release | boundary tests |
| Partial finalize duplicates | remote/PR/comment/close idempotency | retry integration test |
| Worktree leak or unsafe GC | active claim/heartbeat checks; git-aware removal | active-old/symlink tests |
| Public tool-name leak | central public-text validator | no-request Wiremock tests |
| Transcript fills disk | byte cap with drain continuation | noisy-worker test |
| Run artifacts accumulate | age-based GC excluding active/checkpoint runs | retention tests |
| Hermes loader/lifecycle drift | Pin minimum v0.18.2; root adapter uses documented `ctx` APIs; explicit setup/cron lifecycle | isolated real-Hermes install/update/remove test |
| Plugin/docs drift | generated contract fixtures | cross-document test |

### Definition of done

The plan is implemented only when a fresh Hermes install can run after explicit setup with its seeded user-owned bridge, no-argument cron ticks are silent on success, a standalone install fails with a precise missing-worker instruction, every README status field is backed by persisted data, all worker descendants die on timeout/shutdown, retries and claims make progress without waiting for stale reaping, corrupt state is preserved, and the complete release gate in Task 9.2 passes.

---

## Original 46-Task RED/GREEN/REFACTOR Playbook

The task bodies below are preserved from the original plan. Apply the binding overlay above while executing them.


## Phase 0: Project Scaffolding (no Rust code yet)

### Task 0.1: Create Cargo workspace skeleton

**Objective:** Lay down the binary crate skeleton with module stubs and a complete `Cargo.toml` including all dev-dependencies needed by subsequent test code.

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/lib.rs`
- Create: `src/config.rs`
- Create: `src/poll.rs`
- Create: `src/queue.rs`
- Create: `src/worktree.rs`
- Create: `src/worker.rs`
- Create: `src/github.rs`
- Create: `src/finalize.rs`
- Create: `src/status.rs`
- Create: `src/logging.rs`
- Create: `src/prompt.rs`
- Create: `src/error.rs`
- Create: `src/validate.rs`
- Create: `src/verify.rs`
- Create: `src/context.rs`
- Create: `.gitignore`
- Create: `LICENSE` (MIT)

**Step 1:** Write `Cargo.toml` with the dependencies listed in the Tech Stack section above **plus the following dev-dependencies** that test code in later tasks relies on:

```toml
[dev-dependencies]
tempfile = "3"         # tempdir() for filesystem test fixtures
wiremock = "0.6"       # HTTP mock server for github client tests
assert_fs = "1"        # temp file/dir fixtures with ergonomic assertions
predicates = "3"       # composable predicate matchers (used with assert_fs)
```

**Step 2:** Write a minimal `src/main.rs` that calls `caduceus::run()` and prints the version.

**Step 3:** Write `src/lib.rs` with empty `pub mod` declarations for every module listed above (including `prompt`, `error`, `validate`, `verify`, and `context`). Also add the following re-exports so consumers don't need to know which module a type lives in:

```rust
pub mod config;
pub mod context;
pub mod error;
pub mod finalize;
pub mod github;
pub mod logging;
pub mod poll;
pub mod prompt;
pub mod queue;
pub mod status;
pub mod validate;
pub mod verify;
pub mod worktree;
pub mod worker;

// Re-exports — types used by more than one module.
// `finalize` consumes `worker::WorkerResult`, so re-export it at the
// crate root (see Task 5.3 for the rationale).
pub use crate::error::{CaduceusError, CaduceusResult, VoiceError};
pub use crate::worker::WorkerResult;
```

**Step 4:** Each module file starts with `// TODO: see planning/2026-07-12_220000-caduceus-v0.1.md task X.Y` placeholder. The `error.rs` module gets a stub `CaduceusError` enum with a `#[allow(dead_code)]` annotation to suppress unused-variant warnings during early phases.

**Step 5:** Write `.gitignore`:

```gitignore
/target/
.worktrees/
*.log
*.heartbeat
state.json
.DS_Store
```

**Step 6:** Verify it builds: `cargo build`. Expected: builds with warnings about unused stub code.

**Step 7:** Commit: `chore: scaffold caduceus binary crate`

### Task 0.2: Hermes plugin adapter and explicit lifecycle

**Objective:** Replace the existing nonfunctional plugin scaffolding with a valid Hermes Agent v0.18.2 directory plugin. Follow Amendment 0.2 and the Hermes plugin compatibility contract exactly.

**Files:**
- Create: `plugin.yaml`
- Create: `__init__.py`
- Create: `skills/caduceus/SKILL.md`
- Create: `plugin-assets/worker-bridge.py`
- Create: `plugin-assets/caduceus-pulse.sh`
- Create: `tests/hermes_plugin_test.py`
- Remove after migration: legacy `plugin/plugin.yaml`, `plugin/SKILL.md`, `plugin/commands/`, and `plugin/cron/`

The checked-in `plugin/` directory is historical scaffolding, not a completed Hermes plugin. Its custom manifest fields, Markdown command convention, profile path, lifecycle hooks, and cron YAML are not consumed by Hermes Agent v0.18.2. Do not preserve those assumptions in implementation.

**RED:** In an isolated `HERMES_HOME` with Hermes Agent 0.18.2 installed, write failing tests for repository-root discovery, exact manifest fields, `register(ctx)` surfaces, non-mutating import, missing-binary status, setup/bridge preservation, and cron reconciliation.

**GREEN:** Implement the root adapter and explicit lifecycle from the binding compatibility contract. Plugin registration is stdlib-only and never builds. `setup` performs the locked Rust build and safe install; `cron-install` uses the public plugin dispatch surface and writes only the sanctioned Hermes script path.

**REFACTOR/verify:** Run the adapter tests against the pinned Hermes version, then `hermes plugins list`, `hermes plugins enable caduceus`, `hermes caduceus doctor`, and an isolated cron install/run/remove smoke test. Verify plugin removal preserves state and the user-owned bridge.

---

## Phase 1: Configuration & Logging

### Task 1.1: Define `Config` struct

**Objective:** Parse `~/.hermes/config.yaml` under the `caduceus:` section into a typed `Config` struct.

**Files:**
- Modify: `src/config.rs`
- Create: `tests/config_test.rs`

**Step 1:** Write failing tests in `tests/config_test.rs`:

```rust
#[test]
fn minimal_config_uses_defaults() {
    let yaml = "caduceus:\n  worker_command: [\"opencode\"]\n";
    let cfg = caduceus::config::Config::from_yaml(yaml).unwrap();
    assert_eq!(cfg.poll_interval_seconds, 120);
    assert_eq!(cfg.poll_user, "your-bot-account");
    assert_eq!(cfg.max_retries_per_issue, 3);
    // Default forbidden strings for the public-voice rule
    assert!(cfg.comment_forbidden_strings.contains(&"caduceus".to_string()));
    assert!(cfg.comment_forbidden_strings.contains(&"opencode".to_string()));
    assert!(cfg.comment_forbidden_strings.contains(&"gentle-ai".to_string()));
    assert!(cfg.comment_forbidden_strings.contains(&"engram".to_string()));
    assert!(!cfg.comment_forbidden_strings.contains(&"sdd".to_string()));
}

#[test]
fn explicit_overrides_win() {
    let yaml = r#"
caduceus:
  poll_interval_seconds: 60
  poll_user: "my-bot"
  worker_command: ["python3", "/path/to/worker.py"]
  worker_timeout_seconds: 1800
  max_retries_per_issue: 5
  comment_forbidden_strings:
    - "custom-internal-tool"
"#;
    let cfg = caduceus::config::Config::from_yaml(yaml).unwrap();
    assert_eq!(cfg.poll_interval_seconds, 60);
    assert_eq!(cfg.poll_user, "my-bot");
    assert_eq!(cfg.worker_timeout_seconds, 1800);
    assert_eq!(cfg.max_retries_per_issue, 5);
    // Explicit override REPLACES defaults (not merges)
    assert_eq!(cfg.comment_forbidden_strings, vec!["custom-internal-tool".to_string()]);
}

#[test]
fn missing_caduceus_section_errors() {
    let yaml = "other_key: foo\n";
    assert!(caduceus::config::Config::from_yaml(yaml).is_err());
}

#[test]
fn paths_are_expanded() {
    let yaml = r#"
caduceus:
  state_dir: "~/.hermes/caduceus-state"
  workdir_base: "~/projects"
  worker_command: ["opencode"]
"#;
    let cfg = caduceus::config::Config::from_yaml(yaml).unwrap();
    assert!(cfg.state_dir.to_string_lossy().contains("/.hermes/caduceus-state"));
    assert!(!cfg.state_dir.to_string_lossy().contains("~"));
}

#[test]
fn feedback_author_allowlist_default_is_empty() {
    let yaml = "caduceus:\n  worker_command: [\"opencode\"]\n";
    let cfg = caduceus::config::Config::from_yaml(yaml).unwrap();
    // Default is empty — users opt-in by listing trusted GitHub logins / IDs.
    assert!(cfg.feedback_author_allowlist.is_empty());
}

#[test]
fn feedback_author_allowlist_parses_logins_and_ids() {
    let yaml = r#"
caduceus:
  worker_command: ["opencode"]
  feedback_author_allowlist:
    - "trusted-maintainer-username"
    - "id:12345678"
"#;
    let cfg = caduceus::config::Config::from_yaml(yaml).unwrap();
    assert_eq!(
        cfg.feedback_author_allowlist,
        vec!["trusted-maintainer-username".to_string(), "id:12345678".to_string()],
    );
}

#[test]
fn comment_ignore_patterns_default_is_standard_bots() {
    let yaml = "caduceus:\n  worker_command: [\"opencode\"]\n";
    let cfg = caduceus::config::Config::from_yaml(yaml).unwrap();
    // Default: filter standard bot accounts so their comments don't pollute
    // the context injected into the worker.
    assert_eq!(cfg.comment_ignore_patterns.len(), 2);
    let joined = cfg.comment_ignore_patterns.join("|");
    assert!(joined.contains("dependabot"));
    assert!(joined.contains("github-actions"));
}

#[test]
fn comment_ignore_patterns_explicit_override_replaces_default() {
    let yaml = r#"
caduceus:
  worker_command: ["opencode"]
  comment_ignore_patterns:
    - "my-org-bot"
"#;
    let cfg = caduceus::config::Config::from_yaml(yaml).unwrap();
    // Same REPLACE-not-merge semantics as comment_forbidden_strings.
    assert_eq!(cfg.comment_ignore_patterns, vec!["my-org-bot".to_string()]);
}
```

**Step 2:** Run tests, verify FAIL: `cargo test --test config_test`. Expected: `Config::from_yaml` not found, plus the new fields `feedback_author_allowlist` and `comment_ignore_patterns` are missing from `Config`.

**Step 3:** Implement `Config` in `src/config.rs`. Use `serde_yaml` with `#[serde(default)]` for every field so missing keys get defaults. Use `std::path::PathBuf` and implement custom deserialization that expands `~` via `shellexpand`. The struct must include (with defaults applied):

- `poll_interval_seconds: u64` — default `120`
- `poll_user: String` — default `"your-bot-account"`
- `state_dir: PathBuf` — default `~/.hermes/caduceus-state` (after `~` expansion)
- `log_path: PathBuf` — default `~/.hermes/caduceus-state/processor.log`
- `workdir_base: PathBuf` — default `~/projects`
- `worker_command: Vec<String>` — **required**, no default (the daemon refuses to start without this — see Task 1.6)
- `worker_timeout_seconds: u64` — default `3600`
- `stale_run_hours: u64` — default `1`
- `max_retries_per_issue: u32` — default `3`
- `ticket_label_code: String` — default `"🤖 auto-fix"`
- `ticket_label_investigation: String` — default `"🤖 auto-fix-investigate"`
- `comment_forbidden_strings: Vec<String>` — default `["caduceus", "opencode", "gentle-ai", "engram"]`. **Explicit user values REPLACE the defaults** — they do not merge. This keeps the rule strict: a user who lists one forbidden string is signaling they have thought about the rule and want that exact list.
- `feedback_author_allowlist: Vec<String>` — default `[]`. Each entry is either a GitHub login or `id:<numeric>` (numeric IDs resist rename spoofing; see Resolved Decision on `feedback_author_allowlist`).
- `comment_ignore_patterns: Vec<String>` — default `[r"dependabot\[bot\]", r"github-actions\[bot\]"]` (two raw-string regex patterns). Same REPLACE-not-merge semantics as `comment_forbidden_strings`.
- `github_token: Option<String>` — no default; falls back to env-var resolution in Task 1.2.
- `api_base: String` — default `"https://api.github.com"` (tests override with `wiremock` URLs; see Task 7.5).
- `dry_run: bool` — default `false`. Parsed from the `CADUCEUS_DRY_RUN` env var by `Config::load()` (Task 1.3), so the flag is in place before `caduceus::run()` enters the main loop. The YAML field is provided as a convenience override for tests (see Task 5.5), but the env var wins when both are set.

**`Config::defaults()`** (referenced by Tasks 1.2, 1.6, 6.6, 7.5) returns a `Config` with all fields populated with the documentation defaults above. It MUST be implemented in this task — it is not optional, every later task that does `..Config::defaults()` will fail to compile without it.

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(config): parse ~/.hermes/config.yaml under caduceus: section`

### Task 1.2: Token resolution hierarchy

**Objective:** Implement `Config::resolve_token()` that tries explicit → `CADUCEUS_GITHUB_TOKEN` → `GITHUB_TOKEN` → `gh auth token` shell-out.

**Files:**
- Modify: `src/config.rs`
- Modify: `tests/config_test.rs`

**Step 1:** Write failing tests:

```rust
#[test]
fn explicit_token_wins() {
    std::env::set_var("GITHUB_TOKEN", "env-token");
    let cfg = Config { github_token: Some("explicit".into()), ..Config::defaults() };
    assert_eq!(cfg.resolve_token().unwrap(), "explicit");
    std::env::remove_var("GITHUB_TOKEN");
}

#[test]
fn env_var_used_when_no_explicit() {
    std::env::set_var("CADUCEUS_GITHUB_TOKEN", "from-env");
    let cfg = Config { github_token: None, ..Config::defaults() };
    assert_eq!(cfg.resolve_token().unwrap(), "from-env");
    std::env::remove_var("CADUCEUS_GITHUB_TOKEN");
}
```

**Step 2:** Run tests, verify FAIL.

**Step 3:** Implement `resolve_token()` following the contract in the README.

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(config): github token resolution hierarchy`

### Task 1.3: Config file resolution chain

**Objective:** Implement the config lookup order: `$CADUCEUS_CONFIG` env var → `~/.hermes/config.yaml` (under `caduceus:` section) → `~/.config/caduceus/config.yaml` standalone fallback.

**Files:**
- Modify: `src/config.rs`
- Modify: `tests/config_test.rs`

**Step 1:** Write failing tests using `tempfile` to create isolated config locations:

```rust
#[test]
fn caduceus_config_env_var_wins() {
    let tmp = tempdir().unwrap();
    let env_path = tmp.path().join("env.yaml");
    std::fs::write(&env_path, "caduceus:\n  poll_user: \"from-env\"\n  worker_command: [\"x\"]\n").unwrap();
    std::env::set_var("CADUCEUS_CONFIG", &env_path);

    let hermes = tempdir().unwrap();
    let hermes_path = hermes.path().join("config.yaml");
    std::fs::write(&hermes_path, "caduceus:\n  poll_user: \"from-hermes\"\n  worker_command: [\"x\"]\n").unwrap();

    let cfg = caduceus::config::Config::load_with_overrides(Some(&hermes_path), Some(&tmp.path().join("standalone.yaml"))).unwrap();
    assert_eq!(cfg.poll_user, "from-env");

    std::env::remove_var("CADUCEUS_CONFIG");
}

#[test]
fn hermes_config_used_when_no_env_var() {
    std::env::remove_var("CADUCEUS_CONFIG");
    let hermes = tempdir().unwrap();
    let path = hermes.path().join("config.yaml");
    std::fs::write(&path, "caduceus:\n  poll_user: \"hermes\"\n  worker_command: [\"x\"]\n").unwrap();

    let cfg = caduceus::config::Config::load_with_overrides(Some(&path), None).unwrap();
    assert_eq!(cfg.poll_user, "hermes");
}

#[test]
fn hermes_fallback_uses_caduceus_subsection() {
    std::env::remove_var("CADUCEUS_CONFIG");
    let hermes = tempdir().unwrap();
    let path = hermes.path().join("config.yaml");
    std::fs::write(&path, "other_section:\n  foo: bar\ncaduceus:\n  poll_user: \"from-hermes\"\n  worker_command: [\"x\"]\n").unwrap();

    let cfg = caduceus::config::Config::load_with_overrides(Some(&path), None).unwrap();
    assert_eq!(cfg.poll_user, "from-hermes");
}

#[test]
fn standalone_used_when_hermes_missing() {
    std::env::remove_var("CADUCEUS_CONFIG");
    let standalone = tempdir().unwrap();
    let path = standalone.path().join("config.yaml");
    std::fs::write(&path, "caduceus:\n  poll_user: \"standalone\"\n  worker_command: [\"x\"]\n").unwrap();

    // hermes_path points to a non-existent file → falls through to standalone
    let missing_hermes = standalone.path().join("does-not-exist.yaml");
    let cfg = caduceus::config::Config::load_with_overrides(Some(&missing_hermes), Some(&path)).unwrap();
    assert_eq!(cfg.poll_user, "standalone");
}
```

**Step 2:** Run tests, verify FAIL.

**Step 3:** Implement `Config::load_with_overrides(hermes_path: Option<&Path>, standalone_path: Option<&Path>) -> Result<Self>` and a higher-level `Config::load() -> Result<Self>` that does the full resolution chain from the decision (env var → Hermes primary → standalone fallback).

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(config): Hermes-primary with standalone fallback resolution chain`

### Task 1.4: Structured logging

**Objective:** Configure `tracing` to write to both stderr (for cron visibility) and the configured `log_path` (for post-mortem).

**Files:**
- Modify: `src/logging.rs`
- Modify: `src/main.rs`

**Step 1:** Write a smoke test:

```rust
#[test]
fn logging_creates_parent_dir() {
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("nested/processor.log");
    caduceus::logging::init(&log_path).unwrap();
    tracing::info!("hello");
    assert!(log_path.exists());
}
```

**Step 2:** Implement `init(log_path: &Path) -> Result<()>` that creates parent dirs and configures a `tracing_subscriber` with both stderr and file sinks.

**Step 3:** Commit: `feat(logging): structured tracing to stderr + file`

### Task 1.5: Unified error type hierarchy

**Objective:** Define a crate-level error enum that every module uses, eliminating ad-hoc error types.

**Files:**
- Modify: `src/error.rs` (stub from Task 0.1)
- Create: `tests/error_test.rs`

**Step 1:** Write failing tests:

```rust
#[test]
fn config_error_converts_from_serde() {
    let bad_yaml = "caduceus: [unclosed list";
    let err: caduceus::error::CaduceusError = serde_yaml::from_str::<caduceus::config::Config>(bad_yaml).unwrap_err().into();
    assert!(matches!(err, caduceus::error::CaduceusError::Config { .. }));
}

#[test]
fn rate_limit_error_display() {
    let err = caduceus::error::CaduceusError::RateLimited { reset_at: 1700000000, remaining: 0 };
    let msg = err.to_string();
    assert!(msg.contains("429") || msg.contains("rate") || msg.contains("Rate"));
}

#[test]
fn io_error_conversion() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let cad_err: caduceus::error::CaduceusError = io_err.into();
    assert!(matches!(cad_err, caduceus::error::CaduceusError::Io(_)));
}

#[test]
fn voice_error_standalone() {
    let err = caduceus::error::VoiceError::Forbidden { found: "Caduceus".into() };
    assert!(err.to_string().contains("Caduceus"));
}
```

**Step 2:** Implement `src/error.rs`:

```rust
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CaduceusError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("GitHub API error: {status} — {message}")]
    GitHubApi { status: u16, message: String },

    #[error("Rate limited (429). Reset at {reset_at}. Remaining: {remaining}")]
    RateLimited { reset_at: u64, remaining: u16 },

    #[error("Token resolution failed: {0}")]
    TokenResolution(String),

    #[error("Worker error: {0}")]
    Worker(String),

    #[error("Worktree error: {0}")]
    Worktree(String),

    #[error("Queue error: {0}")]
    Queue(String),

    #[error("Prompt generation error: {0}")]
    Prompt(String),

    #[error("Dry-run would {action}: {details}")]
    DryRun { action: String, details: String },

    #[error("{0}")]
    Other(String),
}

#[derive(Error, Debug)]
pub enum VoiceError {
    #[error("Comment rejected: forbidden string '{found}' found")]
    Forbidden { found: String },

    #[error("Comment rejected: {0}")]
    Other(String),
}

/// Convenience alias for crate-wide Result.
pub type CaduceusResult<T> = Result<T, CaduceusError>;
```

**Step 3:** Run tests, verify PASS.

**Step 4:** Commit: `feat(error): unified error types with CaduceusError + VoiceError`

### Task 1.6: Worker command validation

**Objective:** Before entering the main loop, verify that `config.worker_command[0]` is on `$PATH` and is executable. This catches configuration typos early — at daemon startup, not after the first issue is claimed.

**Files:**
- Modify: `src/validate.rs`
- Modify: `src/lib.rs` (add `pub mod validate;`)
- Create: `tests/validate_test.rs`

**Step 1:** Write failing tests:

```rust
#[test]
fn existing_command_on_path_validates() {
    assert!(caduceus::validate::command_exists("echo").unwrap());
}

#[test]
fn nonexistent_command_returns_error() {
    let err = caduceus::validate::command_exists("this-command-definitely-does-not-exist-12345").unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[test]
fn validate_worker_command_success() {
    let cfg = caduceus::config::Config {
        worker_command: vec!["echo".into(), "hello".into()],
        ..caduceus::config::Config::defaults()
    };
    assert!(caduceus::validate::worker_command(&cfg).is_ok());
}

#[test]
fn validate_worker_command_failure() {
    let cfg = caduceus::config::Config {
        worker_command: vec!["this-does-not-exist".into()],
        ..caduceus::config::Config::defaults()
    };
    assert!(caduceus::validate::worker_command(&cfg).is_err());
}

#[test]
fn empty_worker_command_errors() {
    let cfg = caduceus::config::Config {
        worker_command: vec![],
        ..caduceus::config::Config::defaults()
    };
    assert!(caduceus::validate::worker_command(&cfg).is_err());
}
```

**Step 2:** Implement `src/validate.rs`:

```rust
use std::path::Path;

pub fn command_exists(name: &str) -> Result<bool, CaduceusError> {
    Ok(which::which(name).is_ok())
}

pub fn worker_command(cfg: &Config) -> Result<(), CaduceusError> {
    // ... implementation
}
```

**Step 3:** Add `which` to `Cargo.toml` dependencies: `which = "6"`.

**Step 4:** Wire `validate::worker_command` into `main.rs` after config loading, before the main loop. If validation fails, log a clear error and exit with code 1.

**Step 5:** Run tests, verify PASS.

**Step 6:** Commit: `feat(validate): worker command PATH validation at startup`

---

## Phase 2: GitHub Polling

### Task 2.1: HTTP client with ETag cache

**Objective:** Wrap `reqwest` to support ETag-aware conditional GETs against the GitHub REST API.

**Files:**
- Modify: `src/github.rs`
- Create: `tests/github_test.rs`

**Step 1:** Write failing tests using a mock HTTP server (`wiremock`):

```rust
#[tokio::test]
async fn etag_cache_avoids_redundant_downloads() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/test").respond_with(
            wiremock::Response::builder()
                .status(200)
                .header("ETag", "\"v1\"")
                .body("body-1")
        )
    ).await;

    let cfg = caduceus::config::Config {
        github_token: Some("token".into()),
        ..caduceus::config::Config::defaults()
    };
    let mut client = caduceus::github::Client::with_config(&cfg);
    let resp1 = client.get_with_etag(mock.uri(), "/test").await.unwrap();
    assert_eq!(resp1.status, 200);
    assert_eq!(resp1.body, "body-1");
    assert!(resp1.from_cache);

    // second call with same ETag should get 304
    mock.register(
        wiremock::get("/test").respond_with(
            wiremock::Response::builder().status(304).body("")
        )
    ).await;
    let resp2 = client.get_with_etag(mock.uri(), "/test").await.unwrap();
    assert_eq!(resp2.status, 304);
    assert!(resp2.from_cache);
}
```

**Step 2:** Implement `Client`. The constructor takes `&Config` (not a raw token string) so it can resolve the token via the Task 1.2 hierarchy and pick up `api_base`:

```rust
pub struct Client {
    http: reqwest::Client,
    token: String,
    api_base: String,
    etag_cache: HashMap<String, String>,
}

impl Client {
    /// Build a Client from the daemon's Config. Internally calls
    /// `Config::resolve_token()` (Task 1.2). Returns `CaduceusError::TokenResolution`
    /// if no token is available (missing explicit field, env vars unset,
    /// `gh auth token` shell-out fails).
    pub fn with_config(cfg: &config::Config) -> Self {
        let token = cfg.resolve_token().expect("github token required");
        Self {
            http: reqwest::Client::new(),
            token,
            api_base: cfg.api_base.clone(),
            etag_cache: HashMap::new(),
        }
    }

    /// Internal helper for tests that don't want to thread a Config through.
    /// Production code should always use `with_config`.
    #[cfg(test)]
    pub fn new(token: String) -> Self { /* test-only convenience */ }
}
```

ETag cache is an in-memory `HashMap<String, String>` mapping URL → ETag. `get_with_etag` returns `HttpResponse { status, body, from_cache }` so callers can detect 304s without parsing status.

**Step 3:** Commit: `feat(github): ETag-aware HTTP client`

### Task 2.2: Issue events polling

**Objective:** Poll `/repos/{owner}/{repo}/issues/events` for a watched list of repos, filter to relevant events (label add of `🤖 auto-fix` / `🤖 auto-fix-investigate`).

**Files:**
- Modify: `src/poll.rs`
- Modify: `src/github.rs`

**Step 1:** Write failing tests:

```rust
#[test]
fn label_added_event_is_relevant() {
    let event = serde_json::json!({
        "type": "IssuesEvent",
        "action": "labeled",
        "label": { "name": "🤖 auto-fix" },
        "issue": { "number": 42 },
        "repository": { "full_name": "owner/repo" }
    });
    // Returns Some(label_name) when the event matches a trigger label,
    // None otherwise. tick() uses the returned name to choose TicketType.
    assert_eq!(
        caduceus::poll::match_trigger_label(&event, "🤖 auto-fix", "🤖 auto-fix-investigate"),
        Some("🤖 auto-fix".to_string()),
    );
}

#[test]
fn investigation_label_matches_investigation_type() {
    let event = serde_json::json!({
        "type": "IssuesEvent",
        "action": "labeled",
        "label": { "name": "🤖 auto-fix-investigate" },
        "issue": { "number": 42 },
        "repository": { "full_name": "owner/repo" }
    });
    assert_eq!(
        caduceus::poll::match_trigger_label(&event, "🤖 auto-fix", "🤖 auto-fix-investigate"),
        Some("🤖 auto-fix-investigate".to_string()),
    );
}

#[test]
fn unrelated_label_is_filtered() {
    let event = serde_json::json!({
        "type": "IssuesEvent",
        "action": "labeled",
        "label": { "name": "bug" },
        "issue": { "number": 42 },
        "repository": { "full_name": "owner/repo" }
    });
    assert_eq!(
        caduceus::poll::match_trigger_label(&event, "🤖 auto-fix", "🤖 auto-fix-investigate"),
        None,
    );
}

#[test]
fn unlabeled_event_is_filtered() {
    let event = serde_json::json!({
        "type": "IssuesEvent",
        "action": "closed",
        "issue": { "number": 42 }
    });
    assert_eq!(
        caduceus::poll::match_trigger_label(&event, "🤖 auto-fix", "🤖 auto-fix-investigate"),
        None,
    );
}
```

**Step 2:** Implement `match_trigger_label` (formerly `is_relevant_event` — renamed because it now returns the matched label, not just a bool, so tick() can pick the right `TicketType`):

```rust
/// If the event is an `IssuesEvent` whose `action` is `labeled` and whose
/// `label.name` matches one of the configured trigger labels, return
/// `Some(matched_label_name)`. Otherwise return `None`.
pub fn match_trigger_label(
    event: &serde_json::Value,
    code_label: &str,
    investigation_label: &str,
) -> Option<String> {
    if event["type"] != "IssuesEvent" { return None; }
    if event["action"] != "labeled" { return None; }
    let name = event["label"]["name"].as_str()?;
    if name == code_label { return Some(name.to_string()); }
    if name == investigation_label { return Some(name.to_string()); }
    None
}
```

**Step 3:** Update `tick()` (Task 7.1) to use the new signature. The full updated loop is in Task 7.1's `tick()` body — the relevant section is:

```rust
if let Some(matched_label) = poll::match_trigger_label(&event, &cfg.ticket_label_code, &cfg.ticket_label_investigation) {
    let issue_number = event["issue"]["number"].as_u64()
        .ok_or_else(|| CaduceusError::Other("event missing issue.number".into()))?;
    let key = format!("{}#{}", repo, issue_number);
    let ticket_type = if matched_label == cfg.ticket_label_investigation {
        queue::TicketType::Investigation
    } else {
        queue::TicketType::Code
    };
    state.with_lock(|s| queue::enqueue(s, &key, ticket_type))?;
}
```

(`event["issue"]["number"]` is JSON-pointer access on the untyped `serde_json::Value` returned by `fetch_events` — NOT field access like `event.issue.number`, which would only work if `event` were a typed struct.)

**Step 4:** Commit: `feat(poll): relevant event filter with matched-label return`

### Task 2.3: Watched repos resolution

**Objective:** On startup, fetch the user's accessible repos via `/user/repos` and cache them.

**Files:**
- Modify: `src/poll.rs`

**Step 1:** Tests (use `&Client` per the C2 convention so the test exercises the full request-pipeline including auth headers):

```rust
use caduceus::config::Config;

#[tokio::test]
async fn list_watched_repos_returns_repo_slugs() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/user/repos")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"[{"full_name": "owner/repo-a"},{"full_name": "owner/repo-b"}]"#))
    ).await;
    let cfg = Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        ..Config::defaults()
    };
    let client = caduceus::github::Client::with_config(&cfg);
    let repos = caduceus::poll::list_watched_repos(&client).await.unwrap();
    assert_eq!(repos, vec!["owner/repo-a".to_string(), "owner/repo-b".to_string()]);
}

#[tokio::test]
async fn list_watched_repos_returns_empty_on_empty_response() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/user/repos")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;
    let cfg = Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        ..Config::defaults()
    };
    let client = caduceus::github::Client::with_config(&cfg);
    let repos = caduceus::poll::list_watched_repos(&client).await.unwrap();
    assert!(repos.is_empty());
}
```

**Step 2:** Implement `list_watched_repos(client: &Client) -> Result<Vec<String>, CaduceusError>` with TTL caching (refresh if cache > 1 hour old). Cache file: `<state_dir>/cache/watched_repos.json`. Schema:

```json
{ "fetched_at": "2026-07-13T10:00:00Z", "repos": ["owner/repo-a", "owner/repo-b"] }
```

```rust
pub async fn list_watched_repos(client: &Client) -> Result<Vec<String>, CaduceusError> {
    // 1. Read `<state_dir>/cache/watched_repos.json` if it exists.
    // 2. If `fetched_at` is < 1 hour old, return cached repos.
    // 3. Otherwise, GET `${client.api_base}/user/repos`, parse response,
    //    extract `full_name` from each repo object, write cache, return.
}
```

**Step 3:** Commit: `feat(poll): watched repos with TTL cache`

### Task 2.4: Rate-limit handling

**Objective:** Honor `X-RateLimit-Remaining: 0` and `429` responses by exiting cleanly without doing further work.

**Files:**
- Modify: `src/github.rs`
- Modify: `src/poll.rs`

**Step 1:** Test:

```rust
#[tokio::test]
async fn rate_limit_returns_specific_error() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/test").respond_with(
            wiremock::Response::builder()
                .status(429)
                .header("X-RateLimit-Reset", "1700000000")
                .body("")
        )
    ).await;
    // Build a minimal Config with a token so resolve_token() succeeds.
    let cfg = caduceus::config::Config {
        github_token: Some("test-token".into()),
        ..caduceus::config::Config::defaults()
    };
    let client = caduceus::github::Client::with_config(&cfg);
    let err = client.get(&mock.uri(), "/test").await.unwrap_err();
    // CaduceusError is the unified error type (see Task 1.5). The local
    // alias `caduceus::github::Error` is re-exported from it so existing
    // call sites read naturally, but there's only one error enum in the
    // crate.
    assert!(matches!(err, caduceus::error::CaduceusError::RateLimited { reset_at: 1700000000, .. }));
}
```

**Step 2:** `Client::get` returns `Result<HttpResponse, CaduceusError>` (see C2 for the constructor). On 429, it constructs `CaduceusError::RateLimited { reset_at, remaining }`. For convenience, `src/github.rs` adds:

```rust
/// Local alias so call sites can write `github::Error` instead of the
/// fully-qualified `error::CaduceusError`. Re-export only — there's only
/// one error type in the crate (Task 1.5).
pub use crate::error::CaduceusError as Error;
```

**Step 3:** Wire `main()` to catch this and exit with code 0 (silent cron — retry next tick).

**Step 4:** Commit: `feat(github): rate-limit detection`

### Task 2.5: Label-removed detection

**Objective:** Before processing a queued issue, verify it still has the trigger label. If the label was removed between queuing and processing, skip it and log a note. This prevents the daemon from working on an issue the user has intentionally unlabeled.

**Files:**
- Modify: `src/verify.rs`
- Modify: `src/lib.rs` (add `pub mod verify;`)
- Create: `tests/verify_test.rs`

**Step 1:** Write failing tests:

```rust
#[tokio::test]
async fn issue_still_has_trigger_label_returns_ok() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"{"labels": [{"name": "🤖 auto-fix"}]}"#))
    ).await;
    let client = caduceus::github::Client::with_config(&caduceus::config::Config {
        github_token: Some("token".into()),
        ..caduceus::config::Config::defaults()
    });
    let result = caduceus::verify::issue_still_has_label(
        &client, &mock.uri(), "owner/repo", 42, "🤖 auto-fix"
    ).await;
    assert!(result.unwrap());
}

#[tokio::test]
async fn issue_missing_trigger_label_returns_false() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"{"labels": [{"name": "bug"}]}"#))
    ).await;
    let client = caduceus::github::Client::with_config(&caduceus::config::Config {
        github_token: Some("token".into()),
        ..caduceus::config::Config::defaults()
    });
    let result = caduceus::verify::issue_still_has_label(
        &client, &mock.uri(), "owner/repo", 42, "🤖 auto-fix"
    ).await;
    assert!(!result.unwrap());
}

#[tokio::test]
async fn issue_404_returns_error() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42")
            .respond_with(wiremock::Response::builder().status(404).body("Not Found"))
    ).await;
    let client = caduceus::github::Client::with_config(&caduceus::config::Config {
        github_token: Some("token".into()),
        ..caduceus::config::Config::defaults()
    });
    let result = caduceus::verify::issue_still_has_label(
        &client, &mock.uri(), "owner/repo", 42, "🤖 auto-fix"
    ).await;
    assert!(result.is_err());
}
```

**Step 2:** Implement `verify.rs`:

```rust
pub async fn issue_still_has_label(
    client: &caduceus::github::Client,
    api_base: &str,
    slug: &str,
    issue_number: u64,
    label_name: &str,
) -> Result<bool, CaduceusError> {
    // GET /repos/{slug}/issues/{issue_number}
    // Parse the labels array, check if any label's name matches label_name
}
```

**Step 3:** Wire into the main loop: after acquiring the queue head but before provisioning the worktree, call `verify::issue_still_has_label`. If it returns `Ok(false)`, log "label removed, skipping" and move on to the next queue entry.

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(verify): detect removed trigger labels before processing`

### Task 2.6: Fetch full issue detail (title, body, labels, comments, timeline) [DONE-IN-THIS-TASK — prereq for 7.1]

**Objective:** Before spawning the worker, the daemon needs the full issue (title, body, labels, comments, timeline) to (a) inject as `CADUCEUS_*` env vars via `sanitized_env` and (b) build `CADUCEUS_CONTEXT_JSON`. `verify::issue_still_has_label` (Task 2.5) only fetches the label array — it does not return body/comments. Without this task, `tick()` calls `worker::spawn(...)` but the spawn function has no way to read the issue's body and title, so `CADUCEUS_ISSUE_BODY` and `CADUCEUS_ISSUE_TITLE` would arrive empty at the worker and the harness would see a blank issue.

**Files:**
- Create: `src/issue.rs`
- Modify: `src/lib.rs` (add `pub mod issue;`)
- Create: `tests/issue_test.rs`

**Step 1:** Write failing tests covering happy path, 404, label parsing, comment pagination (first page only — daemon only reads the most recent comments), and timeline event extraction:

```rust
use caduceus::config::Config;
use caduceus::issue::{fetch_issue_detail, IssueDetail};

#[tokio::test]
async fn fetch_issue_detail_parses_full_issue() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"{
                    "number": 42,
                    "title": "Login bug",
                    "body": "Users can't log in when their session expires.",
                    "labels": [{"name": "bug"}, {"name": "priority-high"}],
                    "user": {"login": "reporter", "id": 999}
                }"#))
    ).await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42/comments")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"[
                    {"user": {"login": "alice", "id": 1001}, "body": "Confirmed on 1.4.2"},
                    {"user": {"login": "dependabot[bot]", "id": 1002}, "body": "Bump dep"}
                ]"#))
    ).await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42/events")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"[
                    {"event": "labeled", "label": {"name": "🤖 auto-fix"}}
                ]"#))
    ).await;

    let cfg = Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        ..Config::defaults()
    };
    let client = caduceus::github::Client::with_config(&cfg);
    let detail = fetch_issue_detail(&client, "owner/repo", 42).await.unwrap();

    assert_eq!(detail.title, "Login bug");
    assert_eq!(detail.body, "Users can't log in when their session expires.");
    assert_eq!(detail.labels, vec!["bug".to_string(), "priority-high".to_string()]);
    assert_eq!(detail.reporter_login.as_deref(), Some("reporter"));
    assert_eq!(detail.comments.len(), 2);
    // Each comment is (login, Some(numeric_id), body). IDs come from the
    // GitHub user.id field — the test below uses fake IDs (1001, 1002) for
    // alice and dependabot[bot]. The actual values aren't important here;
    // what matters is that Some(_) is present so Task 5.6 can match against
    // feedback_author_allowlist numeric-ID entries.
    assert_eq!(detail.comments[0].0, "alice");
    assert_eq!(detail.comments[0].1, Some(1001));
    assert_eq!(detail.comments[1].0, "dependabot[bot]");
    assert_eq!(detail.comments[1].1, Some(1002));
    assert_eq!(detail.timeline.len(), 1);
}

#[tokio::test]
async fn fetch_issue_detail_returns_404_as_error() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42")
            .respond_with(wiremock::Response::builder().status(404).body("Not Found"))
    ).await;
    // Mock the comments/events endpoints so the test doesn't hang on them.
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42/comments")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/42/events")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;

    let cfg = Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        ..Config::defaults()
    };
    let client = caduceus::github::Client::with_config(&cfg);
    let result = fetch_issue_detail(&client, "owner/repo", 42).await;
    assert!(result.is_err(), "404 must surface as CaduceusError::GitHubApi");
}

#[tokio::test]
async fn fetch_issue_detail_handles_empty_body() {
    // GitHub returns "" for issues with no description.
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/1")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"{"number": 1, "title": "Title only", "body": "", "labels": []}"#))
    ).await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/1/comments")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/1/events")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;

    let cfg = Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        ..Config::defaults()
    };
    let client = caduceus::github::Client::with_config(&cfg);
    let detail = fetch_issue_detail(&client, "owner/repo", 1).await.unwrap();
    assert_eq!(detail.title, "Title only");
    assert_eq!(detail.body, "");
    assert!(detail.labels.is_empty());
    assert!(detail.comments.is_empty());
}

#[tokio::test]
async fn fetch_issue_detail_extracts_label_names_only() {
    // The labels array contains full label objects; we only need the name.
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/1")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"{"number": 1, "title": "t", "body": "b", "labels": [
                    {"name": "bug", "color": "ff0000", "id": 1},
                    {"name": "priority-high", "color": "00ff00", "id": 2}
                ]}"#))
    ).await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/1/comments")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;
    mock.register(
        wiremock::get("/repos/owner/repo/issues/1/events")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;

    let cfg = Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        ..Config::defaults()
    };
    let client = caduceus::github::Client::with_config(&cfg);
    let detail = fetch_issue_detail(&client, "owner/repo", 1).await.unwrap();
    assert_eq!(detail.labels, vec!["bug".to_string(), "priority-high".to_string()]);
}
```

**Step 2:** Implement `src/issue.rs`:

```rust
use serde::Deserialize;
use crate::config::Config;
use crate::error::CaduceusError;
use crate::github::Client;

/// Full issue payload fetched before spawning the worker. The daemon
/// needs this so the worker env has the real body/title/labels
/// (the harness reads them from `CADUCEUS_ISSUE_BODY` etc., per Task 5.2).
#[derive(Debug, Clone)]
pub struct IssueDetail {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub reporter_login: Option<String>,
    /// (author_login, author_numeric_id, body) — the numeric ID comes from
    /// the GitHub `user.id` field on each comment. Used by Task 5.6 to
    /// match against `feedback_author_allowlist` numeric-ID entries
    /// (e.g. `id:12345678`). Resolving login → numeric ID requires a
    /// `/users/{login}` call which is cached at startup.
    pub comments: Vec<(String, Option<u64>, String)>,
    pub timeline: Vec<IssueEvent>,        // raw events, kept for Task 5.6
}

/// One timeline event. Most fields are not consumed by v0.1 but the
/// daemon fetches the events array so the data is available for
/// future context-blob enrichments without a second API call.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueEvent {
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub label: Option<IssueLabel>,
    #[serde(default)]
    pub actor: Option<IssueActor>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueLabel {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueActor {
    pub login: String,
}

/// Fetch the issue, its comments (page 1 only), and its timeline events
/// from GitHub. Returns IssueDetail on 200; CaduceusError::GitHubApi
/// on 4xx/5xx. Pagination is intentionally not handled — the daemon
/// only needs the most recent ~30 comments to build context.
pub async fn fetch_issue_detail(
    client: &Client,
    slug: &str,
    issue_number: u64,
) -> Result<IssueDetail, CaduceusError> {
    let api_base = client.api_base();

    // Three parallel fetches. The reqwest client internally pools
    // connections so this doesn't triple our latency.
    let (issue_resp, comments_resp, events_resp) = tokio::try_join!(
        client.get(&api_base, &format!("/repos/{slug}/issues/{issue_number}")),
        client.get(&api_base, &format!("/repos/{slug}/issues/{issue_number}/comments")),
        client.get(&api_base, &format!("/repos/{slug}/issues/{issue_number}/events")),
    )?;

    // Parse the issue response
    #[derive(Deserialize)]
    struct IssueResp {
        number: u64,
        title: String,
        body: Option<String>,
        labels: Vec<RespLabel>,
        user: Option<RespUser>,
    }
    #[derive(Deserialize)]
    struct RespLabel { name: String }
    #[derive(Deserialize)]
    struct RespUser { login: String, id: u64 }

    let issue: IssueResp = serde_json::from_str(&issue_resp.body)
        .map_err(|e| CaduceusError::Other(format!("parse issue: {e}")))?;

    #[derive(Deserialize)]
    struct CommentResp { user: RespUser, body: String }
    let comments_raw: Vec<CommentResp> = serde_json::from_str(&comments_resp.body)
        .map_err(|e| CaduceusError::Other(format!("parse comments: {e}")))?;

    let timeline: Vec<IssueEvent> = serde_json::from_str(&events_resp.body)
        .map_err(|e| CaduceusError::Other(format!("parse events: {e}")))?;

    // Extract each comment's author numeric ID. GitHub's `user` object on
    // each comment carries both `login` and a numeric `id` field — we
    // need the numeric ID for `feedback_author_allowlist` matching in
    // Task 5.6.
    let comments = comments_raw.into_iter()
        .map(|c| (c.user.login, Some(c.user.id), c.body))
        .collect();

    Ok(IssueDetail {
        number: issue.number,
        title: issue.title,
        body: issue.body.unwrap_or_default(),
        labels: issue.labels.into_iter().map(|l| l.name).collect(),
        reporter_login: issue.user.map(|u| u.login),
        comments,
        timeline,
    })
}
```

Add a `pub api_base(&self) -> &str` accessor to `Client` (Task 2.1's Client struct) — needed by `fetch_issue_detail` so it doesn't have to thread `api_base` separately.

**Step 3:** Re-export `IssueDetail` from `src/lib.rs`:

```rust
pub use crate::issue::{IssueDetail, IssueEvent};
```

**Step 4:** Update `tick()` in Task 7.1 to call `fetch_issue_detail` between `verify::issue_still_has_label` and `worktree::create`. The relevant section becomes:

```rust
// 6. Verify the trigger label is still on the issue (Task 2.5).
let still_labeled = verify::issue_still_has_label(
    &client, &cfg.api_base, &entry.key_to_slug(),
    entry.key_to_number(), &cfg.ticket_label_code,
).await?;
if !still_labeled {
    tracing::info!("{}: label removed mid-tick, skipping", entry.key);
    state.with_lock(|s| queue::record_skipped(s, &entry.key))?;
    continue;
}

// 6b. Fetch the full issue detail so the worker env has body/title/labels
//     (Task 2.6). Without this step the harness receives empty issue data.
let issue_detail = issue::fetch_issue_detail(
    &client, &entry.key_to_slug(), entry.key_to_number(),
).await?;

// 7. Provision worktree, spawn worker, finalize (or rollback on
//    failure). The spawn function reads issue_detail to build the env.
let run_id = ulid::Ulid::new().to_string();
let worktree = worktree::create(&cfg, &entry.key, &run_id)?;
let worker_result = worker::spawn(
    &cfg, &issue_detail, &worktree, &run_id,
).await;
```

Note: `worker::spawn`'s signature changes — it now takes `&IssueDetail` instead of `&QueueEntry`. Update Task 7.0's `worker::spawn` stub to match:

```rust
pub async fn spawn(
    cfg: &crate::config::Config,
    issue: &crate::IssueDetail,
    worktree: &std::path::Path,
    run_id: &str,
) -> Result<crate::WorkerResult, CaduceusError> {
    unimplemented!("implemented in Tasks 5.1-5.3")
}
```

The body/title/labels flow: `tick` → `IssueDetail` → `worker::spawn` → `worker::sanitized_env(SanitizedEnvArgs { title: &issue.title, body: &issue.body, ... })`. Comment and timeline data flow: `tick` → `IssueDetail` → `context::build_context_json(&issue.timeline, &issue.comments, ...)` (replaces the `&issue.timeline` / `&issue.comments` placeholders that were already in tick's Step 3 prose).

**Step 5:** Run tests, verify PASS.

**Step 6:** Commit: `feat(issue): fetch full issue detail before spawning worker`

---

## Phase 3: Queue & Atomic Claim

### Task 3.0: Queue data model [DONE-IN-THIS-TASK — prereq for 3.1]

**Objective:** Define the four core data types that the queue module operates on (`QueueEntry`, `QueueState`, `Phase`, `TicketType`) and the serde schema for `state.json`. Without these, every Phase 3 test refers to types that don't exist.

**Files:**
- Modify: `src/queue.rs`
- Create: `tests/queue_model_test.rs`

**Step 1:** Write a failing test that asserts all four types serialize and deserialize cleanly via serde:

```rust
use caduceus::queue::{QueueEntry, QueueState, Phase, TicketType};
use std::collections::HashMap;

#[test]
fn queue_state_round_trips_via_json() {
    let mut entries = HashMap::new();
    entries.insert(
        "owner/repo#42".to_string(),
        QueueEntry {
            key: "owner/repo#42".to_string(),
            phase: Phase::Queued,
            ticket_type: TicketType::Code,
            attempts: 0,
            last_error: None,
            last_run_id: None,
            queued_at: "2026-07-13T10:00:00Z".to_string(),
            updated_at: "2026-07-13T10:00:00Z".to_string(),
        },
    );
    entries.insert(
        "owner/repo#43".to_string(),
        QueueEntry {
            key: "owner/repo#43".to_string(),
            phase: Phase::Failed,
            ticket_type: TicketType::Investigation,
            attempts: 3,
            last_error: Some("worker timeout".to_string()),
            last_run_id: Some("01JCK9X4F7Z8R9W2K3M5N6P7Q8".to_string()),
            queued_at: "2026-07-13T09:00:00Z".to_string(),
            updated_at: "2026-07-13T10:00:00Z".to_string(),
        },
    );

    let state = QueueState { entries, version: 1 };

    let json = serde_json::to_string(&state).unwrap();
    let parsed: QueueState = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(parsed.entries["owner/repo#42"].phase, Phase::Queued);
    assert_eq!(parsed.entries["owner/repo#42"].ticket_type, TicketType::Code);
    assert_eq!(parsed.entries["owner/repo#43"].phase, Phase::Failed);
    assert_eq!(parsed.entries["owner/repo#43"].attempts, 3);
    assert_eq!(
        parsed.entries["owner/repo#43"].last_error.as_deref(),
        Some("worker timeout"),
    );
}

#[test]
fn phase_serializes_as_lowercase_string() {
    // Schema stability: the JSON representation is part of state.json's
    // contract. Lock it down so future serde version bumps can't silently
    // rename "queued" to "Queued" or "QUEUED".
    for (phase, expected) in [
        (Phase::Queued, r#""queued""#),
        (Phase::InProgress, r#""in_progress""#),
        (Phase::Done, r#""done""#),
        (Phase::Failed, r#""failed""#),
    ] {
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(json, expected, "Phase::{:?} serialized as {json}, expected {expected}", phase);
    }
}

#[test]
fn ticket_type_serializes_as_lowercase_string() {
    for (tt, expected) in [
        (TicketType::Code, r#""code""#),
        (TicketType::Investigation, r#""investigation""#),
    ] {
        let json = serde_json::to_string(&tt).unwrap();
        assert_eq!(json, expected);
    }
}

#[test]
fn missing_fields_get_defaults_on_deserialize() {
    // state.json may be edited by hand or migrated from older versions.
    // On load, unknown/missing fields get sensible defaults rather than
    // errors — the daemon survives a forward-compatible schema bump.
    let json = r#"{
        "version": 1,
        "entries": {
            "owner/repo#1": {
                "key": "owner/repo#1",
                "phase": "queued",
                "ticket_type": "code",
                "queued_at": "2026-07-13T10:00:00Z",
                "updated_at": "2026-07-13T10:00:00Z"
            }
        }
    }"#;
    let parsed: QueueState = serde_json::from_str(json).unwrap();
    let entry = &parsed.entries["owner/repo#1"];
    assert_eq!(entry.attempts, 0);
    assert_eq!(entry.last_error, None);
    assert_eq!(entry.last_run_id, None);
}
```

**Step 2:** Implement the four types in `src/queue.rs`. Add `walkdir`, `serde_json`, `chrono` (or `time`) to `Cargo.toml` deps as needed — these are already in the scaffold from Phase 0 for serde purposes; `chrono` for ISO-8601 timestamps may be a new dep added in Task 0.1's Cargo.toml:

```rust
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Lifecycle phase of a queued issue. Transitions:
/// `Queued` → `InProgress` → `Done` (terminal)
///                     → `Failed` (terminal after max_retries_per_issue)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Queued,
    InProgress,
    Done,
    Failed,
}

/// Maps directly from the trigger label the user added:
/// `ticket_label_code` (default "🤖 auto-fix") → Code,
/// `ticket_label_investigation` (default "🤖 auto-fix-investigate") → Investigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketType {
    Code,
    Investigation,
}

/// One row in `state.json`. Keyed by `"owner/repo#number"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub key: String,
    pub phase: Phase,
    pub ticket_type: TicketType,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_run_id: Option<String>,
    pub queued_at: String,   // ISO-8601 UTC
    pub updated_at: String,
}

impl QueueEntry {
    /// Split the key into owner/repo slug + numeric issue number.
    /// `"owner/repo#42" -> ("owner/repo", 42)`. Panics on malformed keys
    /// (the daemon only writes well-formed keys, see `enqueue`).
    pub fn key_to_slug(&self) -> String {
        let hash = self.key.find('#').expect("malformed queue key");
        self.key[..hash].to_string()
    }
    pub fn key_to_number(&self) -> u64 {
        let hash = self.key.find('#').expect("malformed queue key");
        self.key[hash + 1..].parse().expect("malformed queue key")
    }
}

/// The full contents of `<state_dir>/state.json`. Wrapped in a struct
/// (not a bare HashMap) so the file has a stable `version` field for
/// future schema migrations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueState {
    pub version: u32,           // schema version, currently always 1
    pub entries: HashMap<String, QueueEntry>,
}
```

**Step 3:** Re-export the types from `src/lib.rs` so `finalize` and `status` can reference them without depending on `queue.rs` internals:

```rust
// in src/lib.rs
pub use crate::queue::{Phase, QueueEntry, QueueState, TicketType};
```

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(queue): define QueueEntry, QueueState, Phase, TicketType data model`

### Task 3.1: Issue state store

**Objective:** Maintain a `state.json` file at `<state_dir>/state.json` mapping `owner/repo#N` → issue metadata (phase, attempts, last_error, last_run_id, ticket_type).

**Files:**
- Modify: `src/queue.rs`
- Create: `tests/queue_test.rs`

**Step 1:** Tests:

```rust
use caduceus::queue::{self, Phase, TicketType};
use std::collections::HashMap;

#[test]
fn enqueue_and_dequeue_round_trip() {
    let tmp = tempdir().unwrap();
    let store = caduceus::queue::StateStore::new(tmp.path());
    store.ensure_dirs().unwrap();
    store.with_lock(|state| {
        queue::enqueue(state, "owner/repo#1", TicketType::Code);
    }).unwrap();
    let head = store.with_lock(|state| queue::acquire_next(state));
    let acquired = head.unwrap().expect("queue should not be empty after enqueue");
    assert_eq!(acquired.key, "owner/repo#1");
    assert_eq!(acquired.phase, Phase::InProgress);  // acquire_next moves to InProgress
    assert_eq!(acquired.ticket_type, TicketType::Code);
}

#[test]
fn acquire_returns_none_when_empty() {
    let tmp = tempdir().unwrap();
    let store = caduceus::queue::StateStore::new(tmp.path());
    store.ensure_dirs().unwrap();
    let head = store.with_lock(|state| queue::acquire_next(state));
    assert!(head.is_none());
}

#[test]
fn acquire_returns_none_after_head_already_claimed() {
    // Regression: a naive impl that returns the first Queued entry without
    // checking the claim file would re-claim the same issue every tick.
    // acquire_next must mark the entry as InProgress (or skip if a claim
    // file exists) so subsequent ticks move on.
    let tmp = tempdir().unwrap();
    let store = caduceus::queue::StateStore::new(tmp.path());
    store.ensure_dirs().unwrap();
    store.with_lock(|state| {
        queue::enqueue(state, "owner/repo#1", TicketType::Code);
    }).unwrap();
    let _first = store.with_lock(|state| queue::acquire_next(state)).unwrap();
    let second = store.with_lock(|state| queue::acquire_next(state)).unwrap();
    assert!(second.is_none(), "acquire_next must not return the same entry twice");
}

#[test]
fn failed_entries_are_skipped() {
    let tmp = tempdir().unwrap();
    let store = caduceus::queue::StateStore::new(tmp.path());
    store.ensure_dirs().unwrap();
    store.with_lock(|state| {
        // Hand-craft a Phase::Failed entry directly to simulate the
        // Task 3.4 retry-budget transition. acquire_next must skip it.
        let now = chrono::Utc::now().to_rfc3339();
        state.entries.insert(
            "owner/repo#1".into(),
            caduceus::QueueEntry {
                key: "owner/repo#1".into(),
                phase: Phase::Failed,
                ticket_type: TicketType::Code,
                attempts: 3,
                last_error: Some("max retries exceeded".into()),
                last_run_id: None,
                queued_at: now.clone(),
                updated_at: now,
            },
        );
    }).unwrap();
    let head = store.with_lock(|state| queue::acquire_next(state)).unwrap();
    assert!(head.is_none());
}
```

**Step 2:** Implement `StateStore` using `fs2` for `flock`. Lock is acquired/released per operation (not held across process boundaries). The `with_lock` callback receives `&mut QueueState` (defined in Task 3.0) and returns whatever the closure returns:

```rust
pub struct StateStore {
    state_path: PathBuf,
    claims_dir: PathBuf,
}

impl StateStore {
    pub fn new(state_dir: &Path) -> Self { /* ... */ }
    pub fn ensure_dirs(&self) -> Result<(), CaduceusError> { /* creates state_dir and claims_dir */ }

    /// Acquire the state file's flock, read-or-init `QueueState`, run `f`,
    /// serialize + flush `QueueState` back, release lock. If the state file
    /// doesn't exist yet, init with `version: 1, entries: HashMap::new()`.
    pub fn with_lock<F, R>(&self, f: F) -> Result<R, CaduceusError>
    where F: FnOnce(&mut QueueState) -> R { /* ... */ }
}

/// Add a new entry to the queue. If an entry with this key already exists,
/// this is a no-op (the issue is already tracked). The new entry starts
/// in Phase::Queued with attempts=0 and the current timestamp.
pub fn enqueue(state: &mut QueueState, key: &str, ticket_type: TicketType) {
    let now = chrono::Utc::now().to_rfc3339();
    state.entries.entry(key.to_string()).or_insert(QueueEntry {
        key: key.to_string(),
        phase: Phase::Queued,
        ticket_type,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        queued_at: now.clone(),
        updated_at: now,
    });
}

/// Atomically claim the next Queued entry. Returns `None` if the queue is
/// empty or every Queued entry is already claimed (Task 3.2).
/// On success, the entry's phase is set to `InProgress` and its
/// `updated_at` is bumped. The O_EXCL claim file is created in
/// Task 3.2 — this task just returns the entry.
pub fn acquire_next(state: &mut QueueState) -> Option<QueueEntry> { /* ... */ }
```

**Step 3:** Commit: `feat(queue): state store with flock-protected operations`

### Task 3.2: Atomic claim via `O_CREAT | O_EXCL`

**Objective:** Issuing `acquire_next` must atomically create a claim file `<state_dir>/claims/<key>.claim` and only one concurrent process can win.

**Files:**
- Modify: `src/queue.rs`

**Step 1:** Test using two concurrent `acquire_next` calls — only one should succeed:

```rust
use caduceus::queue::{self, TicketType};

#[test]
fn concurrent_acquire_only_one_wins() {
    let tmp = tempdir().unwrap();
    let store = std::sync::Arc::new(caduceus::queue::StateStore::new(tmp.path()));
    store.ensure_dirs().unwrap();
    store.with_lock(|state| {
        queue::enqueue(state, "owner/repo#1", TicketType::Code);
    }).unwrap();

    let s1 = store.clone();
    let s2 = store.clone();
    let h1 = std::thread::spawn(move || s1.with_lock(|state| queue::acquire_next(state)));
    let h2 = std::thread::spawn(move || s2.with_lock(|state| queue::acquire_next(state)));
    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();
    let winners: Vec<_> = [r1, r2].into_iter().filter_map(|x| x.ok().flatten()).collect();
    assert_eq!(winners.len(), 1);
}
```

**Step 2:** Implement `acquire_next` using `OpenOptions::new().create_new(true).write(true).open(claim_path)`. On `AlreadyExists` error, skip to next queue entry.

**Step 3:** Commit: `feat(queue): O_CREAT|O_EXCL atomic claim`

### Task 3.3: Stale claim reaping

**Objective:** On every tick, scan claim files. Any claim older than `stale_run_hours` is reaped: claim file deleted, issue moved back to `Phase::Queued` with `attempts` unchanged.

**Files:**
- Modify: `src/queue.rs`

**Step 1:** Test by creating a claim file with a backdated mtime and verifying reap moves issue back:

```rust
use caduceus::queue::{self, Phase, TicketType};

#[test]
fn reap_stale_claim_returns_entry_to_queued() {
    let tmp = tempdir().unwrap();
    let store = caduceus::queue::StateStore::new(tmp.path());
    store.ensure_dirs().unwrap();
    store.with_lock(|state| {
        queue::enqueue(state, "owner/repo#1", TicketType::Code);
    }).unwrap();

    // Acquire, then backdate the claim file's mtime.
    let acquired = store.with_lock(|state| queue::acquire_next(state)).unwrap().unwrap();
    assert_eq!(acquired.phase, Phase::InProgress);
    let claim_path = tmp.path().join("claims").join("owner/repo#1.claim");
    assert!(claim_path.exists());
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
    filetime::set_file_mtime(&claim_path, filetime::FileTime::from_system_time(old)).unwrap();

    // Run reap with threshold_hours = 1 → the 2-day-old claim is stale.
    queue::reap_stale_claims(&store, 1).unwrap();

    let head = store.with_lock(|state| queue::acquire_next(state)).unwrap();
    let reaped = head.expect("reaped entry should be available for re-acquire");
    assert_eq!(reaped.phase, Phase::Queued);
    assert!(!claim_path.exists(), "stale claim file should be deleted");
}
```

**Step 2:** Implement `reap_stale_claims(state, store, threshold_hours)`.

**Step 3:** Commit: `feat(queue): stale claim reaper`

### Task 3.4: Bounded retry budget

**Objective:** After `max_retries_per_issue` consecutive failures, transition an issue to `failed` phase and stop reclaiming it.

**Files:**
- Modify: `src/queue.rs`

**Step 1:** Test: enqueue an issue, fail it `max_retries + 1` times, verify phase becomes `Phase::Failed` and `acquire_next` returns `None`:

```rust
use caduceus::queue::{self, Phase, TicketType};

#[test]
fn retry_budget_transitions_to_failed_phase() {
    let tmp = tempdir().unwrap();
    let store = caduceus::queue::StateStore::new(tmp.path());
    store.ensure_dirs().unwrap();
    store.with_lock(|state| {
        queue::enqueue(state, "owner/repo#1", TicketType::Code);
    }).unwrap();

    let max_retries = 3u32;
    // Simulate `max_retries + 1` failures by calling record_failure once per tick.
    for _ in 0..=max_retries {
        store.with_lock(|state| {
            queue::record_failure(state, "owner/repo#1", "timeout", max_retries);
        }).unwrap();
    }

    let final_state = store.with_lock(|state| state.clone()).unwrap();
    let entry = &final_state.entries["owner/repo#1"];
    assert_eq!(entry.phase, Phase::Failed);
    assert_eq!(entry.attempts, max_retries + 1);

    let head = store.with_lock(|state| queue::acquire_next(state)).unwrap();
    assert!(head.is_none(), "acquire_next must skip Phase::Failed entries");
}
```

**Step 2:** Implement `record_failure` and skip `Phase::Failed` issues in `acquire_next`:

```rust
/// Increment the entry's attempts counter, record the error message, and
/// transition to `Phase::Failed` if `attempts > max_retries_per_issue`.
/// Caller passes the budget (from `Config::max_retries_per_issue`) so the
/// queue module stays free of config dependencies.
pub fn record_failure(state: &mut QueueState, key: &str, error: &str, max_retries: u32) {
    if let Some(entry) = state.entries.get_mut(key) {
        entry.attempts += 1;
        entry.last_error = Some(error.to_string());
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        if entry.attempts > max_retries {
            entry.phase = Phase::Failed;
        }
    }
}

/// acquire_next filters out: (a) entries with phase != Phase::Queued,
/// (b) entries whose `<state_dir>/claims/<key>.claim` file already exists
/// (Task 3.2). Returns the first surviving entry, or None.
pub fn acquire_next(state: &mut QueueState) -> Option<QueueEntry> {
    // 1. find first entry with phase == Phase::Queued AND no claim file
    // 2. atomically create the claim file (Task 3.2 uses O_CREAT|O_EXCL)
    // 3. set entry.phase = InProgress, bump updated_at
    // 4. return a clone of the entry
    unimplemented!("implemented across Tasks 3.1 and 3.2")
}
```

**Step 3:** Commit: `feat(queue): bounded retry budget`

---

## Phase 4: Worktree Provisioning

### Task 4.1: Repository discovery

**Objective:** Given an `owner/repo` slug, locate the on-disk clone at `<workdir_base>/<owner>/<repo>`.

**Files:**
- Modify: `src/worktree.rs`

**Step 1:** Test with `tempdir` containing a fake `owner/repo/.git/HEAD` structure.

**Step 2:** Implement `find_main_clone(slug: &str) -> Result<PathBuf>`.

**Step 3:** Commit: `feat(worktree): main clone discovery`

### Task 4.2: Worktree creation

**Objective:** Create a fresh worktree at `<workdir_base>/<owner>/<repo>/.worktrees/<branch>/` from the default branch.

**Files:**
- Modify: `src/worktree.rs`

**Step 1:** Test by initializing a temp git repo, calling `create_worktree`, verifying the worktree directory exists and `git status` reports the new branch.

**Step 2:** Use `git2` crate to find default branch, create branch, create worktree.

**Step 3:** Commit: `feat(worktree): isolated worktree creation`

### Task 4.3: Worktree teardown

**Objective:** On worker completion (success or failure), remove the worktree cleanly.

**Files:**
- Modify: `src/worktree.rs`

**Step 1:** Test creates + removes a worktree and verifies the directory is gone and the parent repo's branch list is clean.

**Step 2:** Implement `remove_worktree(worktree_path: &Path)`.

**Step 3:** Commit: `feat(worktree): teardown`

### Task 4.4: Prompt file generation

**Objective:** Before spawning the worker, build `worker-prompt.md` in the worktree root. This file contains the issue title, body, labels, and any context JSON — everything the AI harness needs to understand the task.

**Files:**
- Modify: `src/prompt.rs` (stub from Task 0.1)
- Create: `tests/prompt_test.rs`

**Step 1:** Write failing tests:

```rust
const INVESTIGATION_LABEL: &str = "🤖 auto-fix-investigate";

#[test]
fn prompt_includes_issue_title_and_body() {
    let prompt = caduceus::prompt::build_prompt(
        "Fix the login bug",
        "Users can't log in when their session expires.",
        &["🤖 auto-fix".to_string()],
        "owner/repo",
        42,
        INVESTIGATION_LABEL,
        None,
    ).unwrap();
    assert!(prompt.contains("Fix the login bug"));
    assert!(prompt.contains("Users can't log in when their session expires."));
    assert!(prompt.contains("# Task"));
}

#[test]
fn prompt_includes_labels_section() {
    let prompt = caduceus::prompt::build_prompt(
        "Fix",
        "Body",
        &["🤖 auto-fix".to_string(), "priority-high".to_string()],
        "owner/repo",
        42,
        INVESTIGATION_LABEL,
        None,
    ).unwrap();
    assert!(prompt.contains("🤖 auto-fix"));
    assert!(prompt.contains("priority-high"));
    assert!(prompt.contains("# Labels"));
}

#[test]
fn prompt_mentions_investigation_when_investigation_label_present() {
    let prompt = caduceus::prompt::build_prompt(
        "Investigate performance",
        "App is slow",
        &["🤖 auto-fix-investigate".to_string()],
        "owner/repo",
        43,
        INVESTIGATION_LABEL,
        None,
    ).unwrap();
    assert!(prompt.contains("investigation") || prompt.contains("analysis"));
    assert!(!prompt.contains("fix the issue"));
}

#[test]
fn prompt_does_not_match_substring_of_other_labels() {
    // Regression: an earlier draft used `l.contains("investigate")` which
    // matched any label with that substring (including ad-hoc user labels
    // like "needs-investigation-by-human"). The check must be exact-match
    // against the configured investigation label only.
    let prompt = caduceus::prompt::build_prompt(
        "Fix bug",
        "Description",
        &["🤖 auto-fix".to_string(), "needs-investigation-by-human".to_string()],
        "owner/repo",
        1,
        INVESTIGATION_LABEL,
        None,
    ).unwrap();
    // "🤖 auto-fix" is the fix trigger → Mode section must say fix, not investigation
    assert!(prompt.contains("**fix** ticket"));
    assert!(!prompt.contains("**investigation** ticket"));
}

#[test]
fn prompt_written_to_file() {
    let tmp = tempdir().unwrap();
    let prompt_content = caduceus::prompt::build_prompt(
        "Test", "Body", &["🤖 auto-fix".to_string()], "owner/repo", 1,
        INVESTIGATION_LABEL, None,
    ).unwrap();

    let prompt_path = tmp.path().join("worker-prompt.md");
    caduceus::prompt::write_prompt_file(&prompt_path, &prompt_content).unwrap();
    assert!(prompt_path.exists());
    let contents = std::fs::read_to_string(&prompt_path).unwrap();
    assert!(contents.contains("# Task"));
}
```

**Step 2:** Implement `src/prompt.rs`:

```rust
pub fn build_prompt(
    title: &str,
    body: &str,
    labels: &[String],
    repo_slug: &str,
    issue_number: u64,
    investigation_label: &str,  // exact-match, not substring
    context_json: Option<&str>,
) -> Result<String, CaduceusError> {
    // Exact-match against the configured investigation label. Substring
    // matching would falsely trigger on user labels like
    // "needs-investigation-by-human" — see the regression test above.
    let is_investigation = labels.iter().any(|l| l == investigation_label);

    let mut prompt = String::new();
    prompt.push_str(&format!("# Task: {}\n\n", title));
    prompt.push_str(&format!("**Repository:** {} #{}\n\n", repo_slug, issue_number));
    prompt.push_str("## Description\n\n");
    prompt.push_str(body);
    prompt.push_str("\n\n## Labels\n\n");
    for label in labels {
        prompt.push_str(&format!("- {}\n", label));
    }
    if is_investigation {
        prompt.push_str("\n## Mode\n\nThis is an **investigation** ticket. Analyze the issue and write findings to `findings.md`. Do not make code changes.\n");
    } else {
        prompt.push_str("\n## Mode\n\nThis is a **fix** ticket. Resolve the issue, edit code files, and write changes. Document what you did in `worker-result.json`.\n");
    }
    if let Some(ctx) = context_json {
        prompt.push_str(&format!("\n## Context\n\n```json\n{}\n```\n", ctx));
    }
    Ok(prompt)
}

pub fn write_prompt_file(path: &Path, content: &str) -> Result<(), CaduceusError> {
    std::fs::write(path, content)?;
    Ok(())
}
```

**Step 3:** Wire prompt generation into the main loop: after worktree creation but before worker spawn. The call site is:

```rust
let prompt_text = prompt::build_prompt(
    &issue.title,
    &issue.body,
    &issue.labels,
    &format!("{}/{}", issue.owner, issue.repo),
    issue.number,
    &cfg.ticket_label_investigation,  // exact-match label, not substring
    Some(&context_json_str),
)?;
prompt::write_prompt_file(&worktree.join("worker-prompt.md"), &prompt_text)?;
```

The prompt file path is `<worktree>/worker-prompt.md`.

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(prompt): generate worker-prompt.md from issue data`

### Task 4.5: Worktree GC subcommand

**Objective:** Implement `caduceus worktree-gc` that prunes orphaned `.worktrees/` directories older than 7 days. This is the safety net for crashed ticks that leave worktrees behind.

**Files:**
- Modify: `src/worktree.rs`
- Create: `tests/worktree_gc_test.rs`

**Step 1:** Write failing tests:

```rust
#[test]
fn gc_removes_orphan_worktrees_older_than_threshold() {
    // Realistic layout: workdir_base/<owner>/<repo>/.worktrees/<branch>
    let tmp = tempdir().unwrap();
    let repo_dir = tmp.path().join("owner").join("repo");
    let orphan = repo_dir.join(".worktrees").join("stale-branch");
    std::fs::create_dir_all(&orphan).unwrap();
    let eight_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86400);
    filetime::set_file_mtime(&orphan, filetime::FileTime::from_system_time(eight_days_ago)).unwrap();

    let removed = caduceus::worktree::gc_orphans(tmp.path(), 7).unwrap();
    assert_eq!(removed, 1);
    assert!(!orphan.exists());
}

#[test]
fn gc_skips_recent_worktrees() {
    let tmp = tempdir().unwrap();
    let repo_dir = tmp.path().join("owner").join("repo");
    let recent = repo_dir.join(".worktrees").join("recent-branch");
    std::fs::create_dir_all(&recent).unwrap();

    let removed = caduceus::worktree::gc_orphans(tmp.path(), 7).unwrap();
    assert_eq!(removed, 0);
    assert!(recent.exists());
}

#[test]
fn gc_handles_missing_worktrees_dir() {
    let tmp = tempdir().unwrap();
    // No owner/repo/.worktrees/ at all — gc must return 0, not error.
    let removed = caduceus::worktree::gc_orphans(tmp.path(), 7).unwrap();
    assert_eq!(removed, 0);
}

#[test]
fn gc_walks_recursively_across_multiple_repos() {
    // Verifies the recursive walk: orphan worktrees in different repos
    // are all found. A non-recursive glob would miss the second repo.
    let tmp = tempdir().unwrap();
    let r1 = tmp.path().join("owner1").join("repo1").join(".worktrees").join("old1");
    let r2 = tmp.path().join("owner2").join("repo2").join(".worktrees").join("old2");
    std::fs::create_dir_all(&r1).unwrap();
    std::fs::create_dir_all(&r2).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
    filetime::set_file_mtime(&r1, filetime::FileTime::from_system_time(old)).unwrap();
    filetime::set_file_mtime(&r2, filetime::FileTime::from_system_time(old)).unwrap();

    let removed = caduceus::worktree::gc_orphans(tmp.path(), 7).unwrap();
    assert_eq!(removed, 2);
    assert!(!r1.exists());
    assert!(!r2.exists());
}
```

**Step 2:** Implement `gc_orphans(workdir_base: &Path, max_age_days: u64) -> Result<usize, CaduceusError>` using `walkdir::WalkDir` to find any directory matching `**/.worktrees/*` (recursive across all repos under `workdir_base`):

```rust
use walkdir::WalkDir;

pub fn gc_orphans(workdir_base: &Path, max_age_days: u64) -> Result<usize, CaduceusError> {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(max_age_days * 86400);
    let mut removed = 0usize;
    // Recursive walk finds every `<owner>/<repo>/.worktrees/<branch>` dir
    // at any depth. The depth-3 filter excludes the `.worktrees/` parent
    // dirs themselves (we want the branch subdirs only).
    for entry in WalkDir::new(workdir_base)
        .min_depth(3)   // skip workdir_base, owner/, repo/
        .max_depth(4)   // owner/repo/.worktrees/<branch>
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        // Only remove if the path's parent directory name is `.worktrees`.
        // This avoids removing unrelated dirs at depth 4.
        let parent_is_worktrees = path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n == ".worktrees")
            .unwrap_or(false);
        if !parent_is_worktrees {
            continue;
        }
        // Only remove if the dir is older than the cutoff.
        let mtime = match entry.metadata().ok().and_then(|m| m.modified().ok()) {
            Some(t) => t,
            None => continue,
        };
        if mtime >= cutoff {
            continue;  // recent — leave it alone
        }
        std::fs::remove_dir_all(path)?;
        removed += 1;
    }
    Ok(removed)
}
```

**Step 3:** Wire as a `clap` subcommand: `caduceus worktree-gc [--dry-run]`. The `--dry-run` flag prints what would be removed without actually deleting.

**Step 4:** Add `filetime` to `Cargo.toml` dependencies: `filetime = "0.2"`.

**Step 5:** Run tests, verify PASS.

**Step 6:** Commit: `feat(worktree): caduceus worktree-gc subcommand for orphan cleanup`

---

## Phase 5: Worker Execution

### Task 5.0: Finalize module stubs [DONE-IN-THIS-TASK — prereq for 5.5]

**Objective:** Add the four primitive functions that Task 5.5 (`finalize_with_dry_run`) and Task 5.4 (`build_pr_body`) call but that aren't implemented until Phase 6. Without these stubs, Task 5.5 cannot compile and the daemon's "everything except side effects" dry-run path cannot be tested.

**Files:**
- Modify: `src/finalize.rs`

**Step 1:** Implement four function stubs in `src/finalize.rs` with their final signatures (matching what Phase 6 will fill in) but bodies that `unimplemented!()` with a marker comment:

```rust
use std::path::Path;
use crate::WorkerResult;
use crate::error::CaduceusError;

/// Commit all changes in the worktree using the worker-provided message.
/// Real implementation: Task 6.1.
pub fn commit_all(worktree: &Path, _commit_message: &str) -> Result<(), CaduceusError> {
    unimplemented!("commit_all — implemented in Task 6.1")
}

/// Push the worker's branch to the remote `origin`.
/// Real implementation: Task 6.2.
pub fn push_branch(_worktree: &Path, _branch_name: &str) -> Result<(), CaduceusError> {
    unimplemented!("push_branch — implemented in Task 6.2")
}

/// Open a pull request via the GitHub REST API.
/// Real implementation: Task 6.3.
pub async fn create_pull_request(
    _client: &crate::github::Client,
    _api_base: &str,
    _slug: &str,
    _title: &str,
    _body: &str,
    _head: &str,
    _base: &str,
) -> Result<String, CaduceusError> {
    unimplemented!("create_pull_request — implemented in Task 6.3")
}

/// Close an issue with a state transition.
pub async fn close_issue(
    _client: &crate::github::Client,
    _api_base: &str,
    _slug: &str,
    _issue_number: u64,
) -> Result<(), CaduceusError> {
    unimplemented!("close_issue — implemented in Task 6.4")
}
```

**Step 2:** Each Phase 6 task (6.1, 6.2, 6.3, 6.4) replaces exactly one of these stubs with the real implementation. Tasks 6.1-6.4 should not need to add new signatures — the stubs above are the final ones.

**Step 3:** Verify Task 5.5's `finalize_with_dry_run` now compiles: `cargo build`. Expected: builds (with `unimplemented!()` panics at runtime if reached — but the dry-run branch returns *before* calling any stub, so it never panics in production).

**Step 4:** Commit: `chore(finalize): add stub primitives so dry-run path compiles before Phase 6`

---

### Task 5.1: Hard timeout enforcement

**Objective:** Spawn the configured `worker_command` inside the worktree, capture stdout/stderr to a transcript file, and SIGKILL it after `worker_timeout_seconds`.

**Files:**
- Modify: `src/worker.rs`

**Step 1:** Test with a worker command that sleeps 60s and a 2s timeout — verify SIGKILL fires and transcript is written.

**Step 2:** Use `tokio::process::Command` with `tokio::time::timeout`. On timeout, send SIGKILL. Use `tokio::spawn` to drain stdout/stderr concurrently into a file.

**Step 3:** Commit: `feat(worker): hard timeout with transcript capture`

### Task 5.2: Sanitized environment

**Objective:** Build the child process environment containing `CADUCEUS_*` vars and explicitly **not** propagating `GITHUB_TOKEN`, `GH_TOKEN`, etc.

**Files:**
- Modify: `src/worker.rs`

**Step 1:** Test:

```rust
use std::collections::HashMap;

#[test]
fn github_token_not_propagated() {
    std::env::set_var("GITHUB_TOKEN", "secret");
    let env = caduceus::worker::sanitized_env(SanitizedEnvArgs {
        slug: "owner/repo",
        issue_number: 42,
        title: "Title",
        body: "Body",
        labels: &["bug".to_string(), "priority-high".to_string()],
        worktree_path: "/worktree",
        run_id: "RUN_ID",
        context_json: "{}",
        state_dir: "/state",
    });
    assert!(!env.contains_key("GITHUB_TOKEN"));
    assert!(!env.contains_key("GH_TOKEN"));
    assert!(!env.contains_key("AUTO_ISSUE_GITHUB_TOKEN"));
    assert_eq!(env.get("CADUCEUS_ISSUE_NUMBER"), Some(&"42".to_string()));
    // CADUCEUS_ISSUE_LABELS is the comma-joined label list — the bridge
    // (plugin/worker-bridge.py) reads this and the harness can use it to
    // decide whether the issue is investigation vs fix (the bridge also
    // forwards the labels to the harness via subprocess args).
    assert_eq!(
        env.get("CADUCEUS_ISSUE_LABELS").map(String::as_str),
        Some("bug,priority-high"),
    );
    // CADUCEUS_STATE_DIR is set so the bridge can find the heartbeat dir
    // (see plugin/worker-bridge.py). Without it the bridge would fall back
    // to ~/.hermes/caduceus-state which may not match cfg.state_dir.
    assert_eq!(env.get("CADUCEUS_STATE_DIR"), Some(&"/state".to_string()));
    std::env::remove_var("GITHUB_TOKEN");
}

#[test]
fn empty_labels_yields_empty_string() {
    // No labels on the issue → CADUCEUS_ISSUE_LABELS is the empty string,
    // not absent. The bridge's `os.environ.get(..., "")` reads it either
    // way, but absent vars confuse some downstream tools that distinguish
    // "set to empty" from "unset". Always set it.
    let env = caduceus::worker::sanitized_env(SanitizedEnvArgs {
        slug: "owner/repo",
        issue_number: 1,
        title: "t",
        body: "b",
        labels: &[],
        worktree_path: "/worktree",
        run_id: "RUN_ID",
        context_json: "{}",
        state_dir: "/state",
    });
    assert_eq!(env.get("CADUCEUS_ISSUE_LABELS"), Some(&"".to_string()));
}
```

**Step 2:** Implement `sanitized_env` with a `SanitizedEnvArgs` struct (not a 7-arg positional signature — that's a footgun for callers and impossible to read at the call site):

```rust
/// Inputs to `sanitized_env`. Bundle the parameters so call sites can
/// use named fields rather than remembering the positional order.
pub struct SanitizedEnvArgs<'a> {
    pub slug: &'a str,
    pub issue_number: u64,
    pub title: &'a str,
    pub body: &'a str,
    /// Comma-separated labels as Vec<String>. Joined with "," when
    /// injected as `CADUCEUS_ISSUE_LABELS`. Empty Vec → empty string
    /// (always set, never omitted).
    pub labels: &'a [String],
    pub worktree_path: &'a str,
    pub run_id: &'a str,
    pub context_json: &'a str,
    pub state_dir: &'a str,  // required so the bridge can locate its heartbeat dir
}

pub fn sanitized_env(args: SanitizedEnvArgs) -> HashMap<String, String> {
    // Start from the current process env, then strip known-credential vars.
    // The worker should never inherit GITHUB_TOKEN/GH_TOKEN/AUTO_ISSUE_GITHUB_TOKEN
    // from the daemon's environment (the daemon has them; the worker must not).
    let mut env: HashMap<String, String> = std::env::vars()
        .filter(|(k, _)| {
            k != "GITHUB_TOKEN"
                && k != "GH_TOKEN"
                && k != "AUTO_ISSUE_GITHUB_TOKEN"
                && k != "CADUCEUS_GITHUB_TOKEN"
        })
        .collect();

    // Inject the CADUCEUS_* contract vars.
    env.insert("CADUCEUS_ISSUE_NUMBER".into(), args.issue_number.to_string());
    env.insert("CADUCEUS_ISSUE_TITLE".into(), args.title.to_string());
    env.insert("CADUCEUS_ISSUE_BODY".into(), args.body.to_string());
    env.insert("CADUCEUS_ISSUE_REPO".into(), args.slug.to_string());
    // CADUCEUS_ISSUE_LABELS is the comma-joined label list — both the
    // bridge (worker-bridge.py line ~49) and harnesses that read env
    // directly rely on it. Always set, even to "" for issues with no
    // labels — see the empty_labels_yields_empty_string test.
    env.insert("CADUCEUS_ISSUE_LABELS".into(), args.labels.join(","));
    // Labels are ALSO embedded in CADUCEUS_CONTEXT_JSON (Task 5.6) for
    // structured access (with author bodies, timestamps, trust flags).
    // The two env vars serve different consumers.
    env.insert("CADUCEUS_WORKTREE_PATH".into(), args.worktree_path.to_string());
    env.insert("CADUCEUS_RUN_ID".into(), args.run_id.to_string());
    env.insert("CADUCEUS_CONTEXT_JSON".into(), args.context_json.to_string());
    env.insert("CADUCEUS_STATE_DIR".into(), args.state_dir.to_string());

    env
}
```

**Step 3:** Commit: `feat(worker): sanitized environment construction`

### Task 5.3: `worker-result.json` parsing

**Objective:** After a successful exit-0 worker run, parse `<worktree>/worker-result.json` and validate the schema. The schema is harness-agnostic — only the four required fields and the optional `artifacts` object.

**Files:**
- Modify: `src/worker.rs`
- Modify: `src/finalize.rs`

**Step 1:** Write failing tests:

```rust
#[test]
fn valid_minimal_result_parses() {
    let json = r#"{
      "status": "success",
      "summary": "Fixed the bug.",
      "branch_name": "auto/fix-42",
      "commit_message": "fix: bug",
      "pull_request_title": "fix: bug"
    }"#;
    let result = caduceus::worker::WorkerResult::from_json(json).unwrap();
    assert_eq!(result.status, "success");
    assert_eq!(result.branch_name, "auto/fix-42");
    assert!(result.artifacts.is_empty());
}

#[test]
fn result_with_artifacts_parses() {
    let json = r#"{
      "status": "success",
      "summary": "...",
      "branch_name": "auto/fix-42",
      "commit_message": "fix: bug",
      "pull_request_title": "fix: bug",
      "artifacts": {
        "spec_path": "openspec/changes/issue-42/spec.md",
        "test_output": "frontend/coverage/lcov.info"
      }
    }"#;
    let result = caduceus::worker::WorkerResult::from_json(json).unwrap();
    assert_eq!(result.artifacts.len(), 2);
}

#[test]
fn missing_required_field_errors() {
    // No status field
    let json = r#"{
      "summary": "...",
      "branch_name": "auto/fix-42",
      "commit_message": "fix: bug",
      "pull_request_title": "fix: bug"
    }"#;
    assert!(caduceus::worker::WorkerResult::from_json(json).is_err());
}

#[test]
fn status_must_be_success() {
    let json = r#"{
      "status": "failed",
      "summary": "...",
      "branch_name": "auto/fix-42",
      "commit_message": "fix: bug",
      "pull_request_title": "fix: bug"
    }"#;
    let err = caduceus::worker::WorkerResult::from_json(json).unwrap_err();
    // Unified error type: status != "success" surfaces as CaduceusError::Worker.
    // The structured InvalidStatus variant exists only as a string payload
    // inside Worker, not as a separate enum.
    assert!(matches!(err, caduceus::error::CaduceusError::Worker(_)));
}

#[test]
fn empty_summary_errors() {
    let json = r#"{
      "status": "success",
      "summary": "",
      "branch_name": "auto/fix-42",
      "commit_message": "fix: bug",
      "pull_request_title": "fix: bug"
    }"#;
    assert!(caduceus::worker::WorkerResult::from_json(json).is_err());
}

#[test]
fn invalid_json_errors() {
    let json = "{ not valid json";
    assert!(caduceus::worker::WorkerResult::from_json(json).is_err());
}

#[test]
fn missing_file_errors() {
    let path = std::path::Path::new("/tmp/does-not-exist-worker-result.json");
    assert!(caduceus::worker::WorkerResult::from_path(path).is_err());
}
```

**Step 2:** Implement `WorkerResult` struct in `src/worker.rs` with serde-derived fields:

```rust
use std::collections::HashMap;
use serde::Deserialize;
use crate::error::CaduceusError;

/// Parsed `worker-result.json` payload from the harness bridge.
/// Lives in `caduceus::worker::WorkerResult`. Re-exported from the
/// crate root (`caduceus::WorkerResult`) so consumers like
/// `caduceus::finalize::build_pr_body` and `caduceus::finalize::finalize_with_dry_run`
/// can refer to it without depending on the worker module's internals.
#[derive(Debug, Deserialize, Clone)]
pub struct WorkerResult {
    #[serde(default)]
    pub status: String,  // must equal "success"
    pub summary: String,
    pub branch_name: String,
    pub commit_message: String,
    pub pull_request_title: String,
    #[serde(default)]
    pub artifacts: HashMap<String, String>,
}

impl WorkerResult {
    pub fn from_json(json: &str) -> Result<Self, CaduceusError> {
        let result: Self = serde_json::from_str(json)
            .map_err(|e| CaduceusError::Worker(format!("invalid worker-result.json: {e}")))?;
        if result.status != "success" {
            return Err(CaduceusError::Worker(format!(
                "worker-result.json status must be \"success\", got \"{}\"",
                result.status
            )));
        }
        if result.summary.trim().is_empty() {
            return Err(CaduceusError::Worker(
                "worker-result.json summary is empty".into(),
            ));
        }
        Ok(result)
    }

    pub fn from_path(path: &std::path::Path) -> Result<Self, CaduceusError> {
        let json = std::fs::read_to_string(path)?;
        Self::from_json(&json)
    }
}
```

The `from_json` constructor validates that `status == "success"` (so a worker that wrote a non-success result gets an error, not a silent success) and returns `CaduceusError::Worker(...)` with a descriptive message on any validation failure. The `from_path` constructor reads + parses and wraps any failure (missing file, bad JSON, missing field, wrong status) into the same `CaduceusError::Worker(...)` variant — there is no separate `ResultError` enum.

**Step 3:** Run tests, verify PASS.

**Step 4:** Commit: `feat(worker): worker-result.json parsing with schema validation`

### Task 5.4: Surface `artifacts` in finalize flow

**Objective:** When `WorkerResult.artifacts` is non-empty, the finalize step should append a "Worker Artifacts" section to the PR description listing each artifact path (and a link if it's a file in the worktree).

**Files:**
- Modify: `src/finalize.rs`

**Step 1:** Write failing test:

```rust
use caduceus::WorkerResult;
use std::collections::HashMap;

#[test]
fn artifacts_section_appended_to_pr_body() {
    let result = WorkerResult {
        status: "success".into(),
        summary: "Fixed.".into(),
        branch_name: "auto/fix-42".into(),
        commit_message: "fix: bug".into(),
        pull_request_title: "fix: bug".into(),
        artifacts: vec![
            ("spec_path".into(), "openspec/changes/issue-42/spec.md".into()),
            ("test_output".into(), "frontend/coverage/lcov.info".into()),
        ].into_iter().collect(),
    };
    let body = caduceus::finalize::build_pr_body(&result);
    assert!(body.contains("Fixed."));
    assert!(body.contains("## Worker Artifacts"));
    assert!(body.contains("openspec/changes/issue-42/spec.md"));
    assert!(body.contains("frontend/coverage/lcov.info"));
}

#[test]
fn no_artifacts_section_when_empty() {
    let result = WorkerResult {
        status: "success".into(),
        summary: "Fixed.".into(),
        branch_name: "auto/fix-42".into(),
        commit_message: "fix: bug".into(),
        pull_request_title: "fix: bug".into(),
        artifacts: HashMap::new(),
    };
    let body = caduceus::finalize::build_pr_body(&result);
    assert!(!body.contains("## Worker Artifacts"));
}
```

**Step 2:** Implement `build_pr_body(&WorkerResult) -> String` that prepends `result.summary` and appends a "## Worker Artifacts" section if `result.artifacts` is non-empty.

**Step 3:** Wire `build_pr_body` into `finalize::create_pull_request` (Task 6.3).

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(finalize): surface worker artifacts in PR body`

### Task 5.5: Dry-run mode

**Objective:** When `CADUCEUS_DRY_RUN=1` is set, the daemon performs every step of the pipeline except the final side effects: it skips the git push, PR creation, and issue close. Instead it logs what it _would_ have done and writes a dry-run report.

**Files:**
- Modify: `src/finalize.rs`
- Modify: `src/main.rs`
- Create: `tests/dry_run_test.rs`

**Step 1:** Write failing tests:

```rust
use caduceus::WorkerResult;
use caduceus::config::Config;
use std::collections::HashMap;
use std::path::Path;
use caduceus::error::CaduceusError;

#[tokio::test]
async fn dry_run_skips_push() {
    let tmp = tempdir().unwrap();
    let result = WorkerResult {
        status: "success".into(),
        summary: "Fixed.".into(),
        branch_name: "auto/fix-42".into(),
        commit_message: "fix: bug".into(),
        pull_request_title: "fix: bug".into(),
        artifacts: HashMap::new(),
    };
    // The dry-run flag is now on Config (parsed by Config::load from
    // CADUCEUS_DRY_RUN, see Task 1.1/1.3). This unit test sets it directly
    // to avoid touching process-global env state.
    let cfg = caduceus::config::Config {
        dry_run: true,
        ..caduceus::config::Config::defaults()
    };

    let output = caduceus::finalize::finalize_with_dry_run(
        &tmp.path(), &result, "owner/repo", 42, &cfg,
    ).await.unwrap();
    assert!(output.dry_run);
    assert!(output.dry_run_log.contains("[DRY-RUN]"));
    assert!(output.dry_run_log.contains("push"));
}

#[test]
fn dry_run_report_written_to_file() {
    let tmp = tempdir().unwrap();
    // Report generation lives in `finalize` alongside FinalizeOutput — there
    // is no separate `dry_run` module. Keeps the public API surface small.
    let report = caduceus::finalize::generate_dry_run_report(
        "owner/repo", 42, "auto/fix-42",
        "Would create PR: fix: bug",
    );
    let report_path = tmp.path().join("dry-run-report.md");
    std::fs::write(&report_path, &report).unwrap();
    assert!(report_path.exists());
    assert!(report.contains("DRY RUN"));
}
```

**Step 2:** The dry-run flag (`Config::dry_run`) is defined in Task 1.1 alongside the rest of the configuration — parsed from `CADUCEUS_DRY_RUN=1` at startup, defaulting to `false`. In the finalize pipeline, before each destructive action (push, PR, close), check `cfg.dry_run` and log `[DRY-RUN] Would <action>` instead of executing.

```rust
use crate::WorkerResult;  // re-exported from crate root (see src/lib.rs)
use crate::config::Config;
use crate::error::CaduceusError;
use std::collections::HashMap;
use std::path::Path;

pub struct FinalizeOutput {
    pub dry_run: bool,
    pub dry_run_log: String,
    pub pr_url: Option<String>,
}

/// Render the dry-run report that gets written to
/// `<worktree>/caduceus-dry-run.md`. Pure function — no I/O side effects,
/// so the test can call it directly and write the result wherever it wants.
pub fn generate_dry_run_report(
    slug: &str,
    issue_number: u64,
    branch_name: &str,
    summary: &str,
) -> String {
    format!(
        "# Caduceus Dry-Run Report\n\nRepository: {}\nIssue:      #{}\nBranch:     {}\n\n{}\n",
        slug, issue_number, branch_name, summary,
    )
}

pub async fn finalize_with_dry_run(
    worktree: &Path,
    result: &WorkerResult,
    slug: &str,
    issue_number: u64,
    cfg: &Config,
) -> Result<FinalizeOutput, CaduceusError> {
    let dry_run = cfg.dry_run;  // Config holds the flag — Task 1.1 parses CADUCEUS_DRY_RUN env var into it.
    let mut log = String::new();

    if dry_run {
        // Dry-run does NOT call commit_all — the worktree may not even be a
        // git repo in test fixtures, and the whole point of dry-run is to
        // report what *would* happen without mutating anything. The report
        // describes the proposed commit message, branch, and PR title — that's
        // enough for a human reviewer to evaluate the worker's intent.
        log.push_str(&format!("[DRY-RUN] Would commit with message: '{}'\n", result.commit_message));
        log.push_str(&format!("[DRY-RUN] Would push branch '{}' to origin\n", result.branch_name));
        log.push_str(&format!("[DRY-RUN] Would create PR: '{}'\n", result.pull_request_title));
        let report = generate_dry_run_report(
            slug,
            issue_number,
            &result.branch_name,
            &log,
        );
        std::fs::write(worktree.join("caduceus-dry-run.md"), &report)?;
        return Ok(FinalizeOutput { dry_run: true, dry_run_log: log, pr_url: None });
    }

    // Real finalize path — call all four primitives (Task 5.0 stubs, replaced
    // by Phase 6 implementations).
    commit_all(worktree, &result.commit_message)?;
    push_branch(worktree, &result.branch_name)?;
    let pr_url = create_pull_request(/* ... */).await?;
    close_issue(/* ... */).await?;
    Ok(FinalizeOutput { dry_run: false, dry_run_log: log, pr_url: Some(pr_url) })
}
```

**Step 3:** Wire dry-run into the main loop: after successful worker run, call `finalize_with_dry_run` instead of the real finalize.

**Step 4:** Run tests, verify PASS.

**Step 5:** Commit: `feat(dry-run): CADUCEUS_DRY_RUN=1 mode for safe testing`

### Task 5.6: Context JSON construction

**Objective:** Build the `CADUCEUS_CONTEXT_JSON` environment variable — a structured JSON blob compiling the issue's historical timeline, trusted comments (from `feedback_author_allowlist`), and filtered comment threads (filtered by `comment_ignore_patterns`).

**Files:**
- Create: `src/context.rs`
- Modify: `src/lib.rs` (add `pub mod context;`)
- Create: `tests/context_test.rs`

**Step 1:** Write failing tests:

```rust
#[test]
fn empty_context_json_is_valid() {
    let json = caduceus::context::build_context_json(
        &[], &[], &[], &[], &[],
    ).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["issue_timeline"].as_array().unwrap().len(), 0);
    assert_eq!(parsed["trusted_comments"].as_array().unwrap().len(), 0);
}

#[test]
fn trusted_comments_included_by_login() {
    let allowlist_logins = ["trusted-user"];
    let allowlist_ids = [];  // no numeric IDs in this test
    let comments = vec![
        ("trusted-user".into(), "Looks like a bug in the auth module.".into()),
        ("random-user".into(), "I agree.".into()),
    ];
    let json = caduceus::context::build_context_json(
        &[], &comments, &allowlist_logins, &allowlist_ids, &[],
    ).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let trusted = parsed["trusted_comments"].as_array().unwrap();
    assert_eq!(trusted.len(), 1);
    assert_eq!(trusted[0]["author"], "trusted-user");
}

#[test]
fn trusted_comments_included_by_numeric_id() {
    // Numeric IDs from `feedback_author_allowlist: ["id:12345678"]`.
    // The comment author carries their numeric ID via IssueDetail (we look it
    // up via the issue reporter / comment user endpoint). For this test we
    // simulate that lookup by passing (author_login, author_id, body).
    let allowlist_logins = [];
    let allowlist_ids = [12345678u64];
    let comments = vec![
        ("renamed-user".into(), Some(12345678u64), "Original maintainer.".into()),
        ("random-user".into(), Some(99999999u64), "Just passing by.".into()),
    ];
    let json = caduceus::context::build_context_json_with_ids(
        &[], &comments, &allowlist_logins, &allowlist_ids, &[],
    ).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let trusted = parsed["trusted_comments"].as_array().unwrap();
    assert_eq!(trusted.len(), 1, "only the id-matching comment is trusted");
    assert_eq!(trusted[0]["author"], "renamed-user");
    assert_eq!(trusted[0]["author_id"], 12345678);
}

#[test]
fn ignored_users_excluded() {
    let comments = vec![
        ("dependabot[bot]".into(), Some(49699333u64), "Bump dep".into()),
        ("human".into(), Some(42u64), "Real comment".into()),
    ];
    let json = caduceus::context::build_context_json_with_ids(
        &[], &comments, &[], &[], &[r"dependabot\[bot\]"],
    ).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let issue_comments = parsed["issue_comments"].as_array().unwrap();
    assert_eq!(issue_comments.len(), 1);
    assert_eq!(issue_comments[0]["author"], "human");
}
```

**Step 2:** Implement `src/context.rs`:

```rust
use regex::Regex;
use crate::error::CaduceusError;

/// Parse `cfg.feedback_author_allowlist` (a `Vec<String>` where each entry is
/// either a bare login or `id:<numeric>`) into separate slices of logins
/// and numeric IDs. Used by tick() to split the config into the two halves
/// that build_context_json needs.
pub fn split_allowlist(allowlist: &[String]) -> (Vec<String>, Vec<u64>) {
    let mut logins = Vec::new();
    let mut ids = Vec::new();
    for entry in allowlist {
        if let Some(id_str) = entry.strip_prefix("id:") {
            if let Ok(id) = id_str.parse::<u64>() {
                ids.push(id);
                continue;
            }
            // Malformed `id:foo` is logged but doesn't panic. We could
            // also reject it at config-load time (Task 1.1) — for now,
            // silently skip.
        } else {
            logins.push(entry.clone());
        }
    }
    (logins, ids)
}

/// Build context JSON from comments that have only author login + body
/// (no numeric IDs). This is the simple path used by tests and by any
/// caller that doesn't have author IDs available.
pub fn build_context_json(
    timeline: &[IssueEvent],
    comments: &[(String, String)],   // (author_login, body)
    allowlist_logins: &[String],
    allowlist_ids: &[u64],            // empty for this simpler signature
    ignore_patterns: &[String],
) -> Result<String, CaduceusError> {
    let adapted: Vec<(String, Option<u64>, String)> = comments.iter()
        .map(|(a, b)| (a.clone(), None, b.clone()))
        .collect();
    build_context_json_with_ids(timeline, &adapted, allowlist_logins, allowlist_ids, ignore_patterns)
}

/// Build context JSON with full author-ID resolution. This is what
/// `tick()` calls — every comment has the comment author's numeric ID
/// resolved via the GitHub `/users/{login}` endpoint cached in `IssueDetail.comments`.
pub fn build_context_json_with_ids(
    timeline: &[IssueEvent],
    comments: &[(String, Option<u64>, String)],  // (author_login, author_id, body)
    allowlist_logins: &[String],
    allowlist_ids: &[u64],
    ignore_patterns: &[String],
) -> Result<String, CaduceusError> {
    let ignore_regexes: Vec<Regex> = ignore_patterns.iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    let filtered_comments: Vec<_> = comments.iter()
        .filter(|(author, _, _)| !ignore_regexes.iter().any(|r| r.is_match(author)))
        .map(|(author, author_id, body)| {
            // Trusted iff the login matches OR the numeric ID matches
            // (when both are present). If only a login is available
            // (author_id is None), fall back to login-only matching.
            let trusted = allowlist_logins.iter().any(|l| l == author)
                || author_id.map(|id| allowlist_ids.contains(&id)).unwrap_or(false);
            serde_json::json!({
                "author": author,
                "author_id": author_id,
                "body": body,
                "trusted": trusted,
            })
        })
        .collect();

    let ctx = serde_json::json!({
        "issue_timeline": timeline,
        "issue_comments": filtered_comments,
        "trusted_comment_authors": {
            "logins": allowlist_logins,
            "ids": allowlist_ids,
        },
    });

    Ok(serde_json::to_string(&ctx)?)
}
```

The numeric-ID lookup itself happens in Task 2.6's `fetch_issue_detail`: GitHub's `GET /repos/{slug}/issues/{n}/comments` returns each comment with a `user` object that includes both `login` and a numeric `id` field (e.g., `"user": {"login": "alice", "id": 12345, ...}`). The fetcher extracts both and packs them into `IssueDetail.comments` as `(login, Some(id), body)`.

**Step 3:** Wire into the main loop: after polling issue details but before spawning the worker, pass the context JSON string as `context_json` field of `SanitizedEnvArgs` (Task 5.2). Concretely:

```rust
let (allowlist_logins, allowlist_ids) =
    context::split_allowlist(&cfg.feedback_author_allowlist);
let context_json = context::build_context_json_with_ids(
    &issue_detail.timeline,
    &issue_detail.comments,  // IssueDetail.comments is Vec<(login, Option<id>, body)>
    &allowlist_logins,
    &allowlist_ids,
    &cfg.comment_ignore_patterns,
)?;
let env = worker::sanitized_env(worker::SanitizedEnvArgs {
    slug: &format!("{}/{}", issue_detail.owner_or_slug, issue_detail.repo),
    issue_number: issue_detail.number,
    title: &issue_detail.title,
    body: &issue_detail.body,
    labels: &issue_detail.labels,
    worktree_path: worktree.to_str().unwrap(),
    run_id: &run_id,
    context_json: &context_json,
    state_dir: cfg.state_dir.to_str().unwrap(),
});
```

**Step 4:** Add `regex` to `Cargo.toml` dependencies: `regex = "1"`.

**Step 5:** Run tests, verify PASS.

**Step 6:** Commit: `feat(context): build CADUCEUS_CONTEXT_JSON with timeline, trusted comments, and numeric-ID allowlist`

---

## Phase 6: Finalization (Branch, Push, PR)

### Task 6.1: Commit changes

**Objective:** In the worktree, commit all changes with the worker-provided `commit_message`.

**Files:**
- Modify: `src/finalize.rs`

**Step 1:** Test creates a worktree with a modified file, calls `commit_all(worktree, msg)`, verifies a new commit on the expected branch.

**Step 2:** Use `git2` to add all files, create commit, get OID.

**Step 3:** Commit: `feat(finalize): commit worker changes`

### Task 6.2: Push branch

**Objective:** Push the new branch to the remote `origin`.

**Files:**
- Modify: `src/finalize.rs`

**Step 1:** Test against a local bare-repo-as-origin fixture (no actual network push).

**Step 2:** Use `git2` to set up the remote ref and push.

**Step 3:** Commit: `feat(finalize): push branch to origin`

### Task 6.3: Pull request creation

**Objective:** Use the GitHub REST API to open a PR with title and body derived from `WorkerResult` (`pull_request_title` for the title, `summary` + optional artifacts section for the body — see Task 5.4).

**Files:**
- Modify: `src/finalize.rs`
- Modify: `src/github.rs`

**Step 1:** Test with `wiremock`:

```rust
#[tokio::test]
async fn pr_creation_posts_to_correct_endpoint() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::post("/repos/owner/repo/pulls")
            .respond_with(wiremock::Response::builder()
                .status(201)
                .body(r#"{"html_url": "https://github.com/owner/repo/pull/1"}"#))
    ).await;
    let client = caduceus::github::Client::with_config(&caduceus::config::Config {
        github_token: Some("token".into()),
        ..caduceus::config::Config::defaults()
    });
    let url = client.create_pull_request(&mock.uri(), "owner/repo", "PR title", "body", "head:branch", "main").await.unwrap();
    assert_eq!(url, "https://github.com/owner/repo/pull/1");
}
```

**Step 2:** Implement `create_pull_request`.

**Step 3:** Commit: `feat(finalize): PR creation via REST API`

### Task 6.4: Issue closure

**Objective:** On successful PR creation, post a completion comment and close the issue.

**Files:**
- Modify: `src/finalize.rs`
- Modify: `src/github.rs`

**Step 1:** Test the comment + close endpoints are called in order.

**Step 2:** Implement and wire into finalize flow.

**Step 3:** Commit: `feat(finalize): completion comment + issue close`

### Task 6.5: Failure handling — don't close, post findings

**Objective:** On non-zero exit code, post a failure comment (with the transcript link) and leave the issue open.

**Files:**
- Modify: `src/finalize.rs`

**Step 1:** Test the failure-path comment.

**Step 2:** Implement. Use the bot-account comment template (matches the public-voice rule — see Resolved Decision 5 above).

**Step 3:** Commit: `feat(finalize): failure-path comment with transcript link`

### Task 6.6: Enforce the public-voice rule on every bot comment

**Objective:** Every comment the daemon posts on an issue or PR must be scanned against `config.comment_forbidden_strings` before posting. If a forbidden string is found, the post is rejected and an error is logged. This is the hard-rule enforcement of the public-voice principle.

**Files:**
- Modify: `src/finalize.rs`
- Modify: `src/github.rs`
- Create: `tests/voice_rule_test.rs`

**Step 1:** Write failing tests:

```rust
use caduceus::error::VoiceError;

#[test]
fn clean_comment_passes_voice_check() {
    let cfg = caduceus::config::Config::defaults();
    let body = "Picking this up — I'll be back with a fix shortly.";
    assert!(caduceus::finalize::voice_check(body, &cfg).is_ok());
}

#[test]
fn comment_mentioning_daemon_name_is_rejected() {
    let cfg = caduceus::config::Config::defaults();
    let body = "Caduceus picked this up. Will report back.";
    let err = caduceus::finalize::voice_check(body, &cfg).unwrap_err();
    // VoiceError lives in caduceus::error (Task 1.5). finalize re-exports
    // it for ergonomics but the canonical path is caduceus::error::VoiceError.
    assert!(matches!(err, caduceus::error::VoiceError::Forbidden { .. }));
}

#[test]
fn comment_mentioning_opencode_is_rejected() {
    let cfg = caduceus::config::Config::defaults();
    let body = "Running an opencode session to fix this.";
    assert!(caduceus::finalize::voice_check(body, &cfg).is_err());
}

#[test]
fn case_insensitive_match() {
    let cfg = caduceus::config::Config::defaults();
    let body = "CADUCEUS is on the case.";
    assert!(caduceus::finalize::voice_check(body, &cfg).is_err());
}

#[test]
fn custom_forbidden_list_overrides_defaults() {
    let cfg = caduceus::config::Config {
        comment_forbidden_strings: vec!["acme-internal".into()],
        ..caduceus::config::Config::defaults()
    };
    // Default strings no longer forbidden when user overrode
    let body1 = "Caduceus picked this up.";
    assert!(caduceus::finalize::voice_check(body1, &cfg).is_ok());
    // Custom string IS forbidden
    let body2 = "Reached out to acme-internal for context.";
    assert!(caduceus::finalize::voice_check(body2, &cfg).is_err());
}
```

**Step 2:** Run tests, verify FAIL.

**Step 3:** Implement `voice_check(body: &str, cfg: &Config) -> Result<(), VoiceError>` where `VoiceError` is the canonical `caduceus::error::VoiceError` (re-exported from `src/lib.rs`). The matcher is **case-insensitive substring** (per the README's "Default forbidden strings" — substring is simpler and safer than word boundaries, and we explicitly want to catch `CADUCEUS`, `Caduceus`, `caduceus` all the same). Reads `cfg.comment_forbidden_strings` (Task 1.1's defaults — `["caduceus", "opencode", "gentle-ai", "engram"]` — apply when the user hasn't overridden).

**Step 4:** Wire `voice_check` into every call to `github_api::post_comment` and `github_api::update_comment`. If the check fails, log an error and skip the post. Do NOT silently fall through and post anyway.

**Step 5:** Run tests, verify PASS.

**Step 6:** Commit: `feat(voice-rule): enforce forbidden-string list on every bot comment`

**Step 7:** Update `README.md` to reference Task 6.6 explicitly under "Hard Rule" note in Decision 5.

---

## Phase 7: Orchestration & Status

### Task 7.0: Orchestrator glue — helper signatures [DONE-IN-THIS-TASK — prereq for 7.1]

**Objective:** Task 7.1's `tick()` orchestrator calls helper functions from `poll`, `queue`, `worktree`, and `worker` that don't have explicit tasks elsewhere in the plan. Without this task, Task 7.1's references to `poll::fetch_events`, `queue::record_skipped`, `queue::mark_done`, `worktree::create`, `worktree::remove`, and `worker::spawn` are undeclared. Tasks 4.1-4.3 (worktree) and 5.1-5.3 (worker) define the *internals* of these helpers; this task defines their public signatures so 7.1 compiles.

**Files:**
- Modify: `src/poll.rs`
- Modify: `src/queue.rs`
- Modify: `src/worktree.rs`
- Modify: `src/worker.rs`

**Step 1:** Add the following function signatures. Tasks 4.1-4.3 fill in `worktree::create` / `worktree::remove`; Tasks 5.1-5.3 fill in `worker::spawn`. Tasks 3.x and 7.1 fill in the `poll` and `queue` helpers:

```rust
// In src/poll.rs:
/// Fetch recent events for a single repo. Returns Vec<serde_json::Value>
/// because the daemon only reads a few fields per event; the GitHub API
/// has ~40 fields per event and parsing all of them is wasted work.
pub async fn fetch_events(
    client: &crate::github::Client,
    api_base: &str,
    repo: &str,
) -> Result<Vec<serde_json::Value>, CaduceusError> {
    unimplemented!("implemented in Task 2.2 follow-up — see plan note")
}

// In src/queue.rs:
/// Mark an entry as skipped (label removed mid-run, etc.) without
/// incrementing attempts. Does not transition phase — stays at InProgress
/// so the reap loop knows to skip it.
pub fn record_skipped(state: &mut QueueState, key: &str) {
    if let Some(entry) = state.entries.get_mut(key) {
        entry.last_error = Some("label removed mid-tick".into());
        entry.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

/// Mark an entry as successfully done. Sets phase, records the run_id,
/// clears last_error.
pub fn mark_done(state: &mut QueueState, key: &str, run_id: &str) {
    if let Some(entry) = state.entries.get_mut(key) {
        entry.phase = Phase::Done;
        entry.last_run_id = Some(run_id.to_string());
        entry.last_error = None;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

// In src/worktree.rs:
/// Create the worktree for this entry's branch and return the path.
/// The branch is `auto/fix-<number>` for TicketType::Code or
/// `auto/investigate-<number>` for TicketType::Investigation.
pub fn create(cfg: &crate::config::Config, key: &str, run_id: &str)
    -> Result<std::path::PathBuf, CaduceusError>
{
    unimplemented!("implemented in Tasks 4.1-4.2")
}

/// Remove the worktree after a tick completes (success or failure).
pub fn remove(worktree: &std::path::Path) -> Result<(), CaduceusError> {
    unimplemented!("implemented in Task 4.3")
}

// In src/worker.rs:
/// Spawn the configured worker_command in the worktree, with the
/// hard timeout from cfg.worker_timeout_seconds. Returns the parsed
/// WorkerResult on exit-0, or an error on non-zero exit / timeout /
/// spawn failure.
pub async fn spawn(
    cfg: &crate::config::Config,
    entry: &crate::QueueEntry,
    worktree: &std::path::Path,
    run_id: &str,
) -> Result<crate::WorkerResult, CaduceusError> {
    unimplemented!("implemented in Tasks 5.1-5.3")
}
```

**Step 2:** Verify Task 7.1's `tick()` body compiles: `cargo build`. Expected: builds (with `unimplemented!()` panics at runtime if reached — but the integration test in Task 7.5 drives every code path with stubs/mocks, so no path is reached without a corresponding implementation existing by then).

**Step 3:** Commit: `chore(orchestrator): declare helper signatures for tick() to call into`

---

### Task 7.1: Main loop

**Objective:** Wire polling → queue → worktree → worker → finalize into one tick.

**Files:**
- Modify: `src/main.rs`
- Modify: `src/lib.rs`

**Step 1:** Implement two entry points in `src/lib.rs`:

```rust
/// Run a single poll-and-process tick using config loaded from disk.
/// Invoked by `bin/caduceus` with no subcommand (the cron profile uses
/// `script: "../bin/caduceus"` with no args, so no-args must default to
/// Run — this is the contract documented in `plugin/cron/caduceus-pulse.yaml`).
pub fn run() -> Result<u8, CaduceusError> {
    let cfg = config::Config::load()?;
    logging::init(&cfg.log_path)?;
    validate::worker_command(&cfg)?;
    // Both run() and run_with_config() route through tick(). The cron
    // invocation path is identical to the test path — only the config
    // source differs.
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| CaduceusError::Other(format!("tokio runtime: {e}")))?;
    runtime.block_on(tick(cfg))
}

/// Test-only entry point: run a single tick with an in-memory Config
/// instead of loading from disk. Used by the integration test (Task 7.5)
/// to drive the daemon against a mocked GitHub API and a temp state dir
/// without touching the host filesystem or the user's actual config.
pub async fn run_with_config(cfg: config::Config) -> Result<u8, CaduceusError> {
    logging::init(&cfg.log_path)?;
    validate::worker_command(&cfg)?;
    tick(cfg).await
}
```

The full `tick()` body (shared by both entry points) is the actual orchestrator:

```rust
pub async fn tick(cfg: config::Config) -> Result<u8, CaduceusError> {
    let client = github::Client::with_config(&cfg);
    let state = queue::StateStore::new(&cfg.state_dir);
    state.ensure_dirs()?;

    // 1. Reap stale claims FIRST so a crashed previous tick's work is
    //    requeued before this tick decides what to work on.
    queue::reap_stale_claims(&state, cfg.stale_run_hours)?;

    // 2. Discover watched repos. The cached TTL path means idle ticks
    //    hit GitHub only on cache miss (see Task 2.3).
    let repos = poll::list_watched_repos(&client).await?;

    // 3. For each repo, fetch recent events and enqueue the relevant ones.
    //    Each enqueue is wrapped in a separate flock so a slow write to
    //    one entry doesn't block other repos.
    for repo in &repos {
        let events = poll::fetch_events(&client, &cfg.api_base, repo).await?;
        for event in events {
            let Some(matched_label) = poll::match_trigger_label(
                &event,
                &cfg.ticket_label_code,
                &cfg.ticket_label_investigation,
            ) else { continue };
            // `event["issue"]["number"]` is JSON-pointer access on the
            // untyped serde_json::Value returned by fetch_events.
            let issue_number = event["issue"]["number"].as_u64()
                .ok_or_else(|| CaduceusError::Other(
                    "event missing issue.number".into(),
                ))?;
            let key = format!("{}#{}", repo, issue_number);
            // Map the matched label to the typed ticket variant (Task 3.0).
            let ticket_type = if matched_label == cfg.ticket_label_investigation {
                queue::TicketType::Investigation
            } else {
                queue::TicketType::Code
            };
            state.with_lock(|s| queue::enqueue(s, &key, ticket_type))?;
        }
    }

    // 4. Honor GitHub's X-RateLimit-Remaining: if exhausted, exit cleanly
    //    with code 0 (cron will retry next tick). Without this, a tight
    //    cron loop + 429 storm could hammer GitHub indefinitely.
    if client.last_remaining() == Some(0) {
        tracing::info!("Rate limit exhausted, exiting tick early");
        return Ok(0);
    }

    // 5. Process the queue head. One issue per tick (concurrency cap = 1,
    //    see Risks table). Loop bounded so a hot queue doesn't starve
    //    signal handling (Task 7.4).
    const MAX_PER_TICK: u8 = 1;
    for _ in 0..MAX_PER_TICK {
        let acquired = state.with_lock(|s| queue::acquire_next(s))?;
        let Some(entry) = acquired else { break };

        // 6. Verify the trigger label is still on the issue (Task 2.5).
        //    If the user removed the label between enqueue and now, skip.
        let still_labeled = verify::issue_still_has_label(
            &client, &cfg.api_base, &entry.key_to_slug(),
            entry.key_to_number(), &cfg.ticket_label_code,
        ).await?;
        if !still_labeled {
            tracing::info!("{}: label removed mid-tick, skipping", entry.key);
            state.with_lock(|s| queue::record_skipped(s, &entry.key))?;
            continue;
        }

        // 7. Provision worktree, spawn worker, finalize (or rollback on
        //    failure). Each phase is wrapped in its own function so the
        //    8 phases below stay legible.
        let run_id = ulid::Ulid::new().to_string();
        let worktree = worktree::create(&cfg, &entry.key, &run_id)?;
        let worker_result = worker::spawn(
            &cfg, &entry, &worktree, &run_id,
        ).await;
        match worker_result {
            Ok(result) => {
                finalize::finalize_with_dry_run(
                    &worktree, &result, &entry.key_to_slug(),
                    entry.key_to_number(), &cfg,
                ).await?;
                state.with_lock(|s| queue::mark_done(s, &entry.key, &run_id))?;
            }
            Err(e) => {
                tracing::error!("{}: worker failed: {e}", entry.key);
                state.with_lock(|s| {
                    queue::record_failure(s, &entry.key, &e.to_string(), cfg.max_retries_per_issue);
                })?;
            }
        }
        worktree::remove(&worktree)?;
    }

    Ok(0)
}
```

Notes on the helper functions called above:

- `entry.key_to_slug()` / `entry.key_to_number()` — small accessors on `QueueEntry` (Task 3.0) that split `"owner/repo#42"` into `("owner/repo", 42)`. Implement these as inherent methods on `QueueEntry` in Task 3.0's Step 2.
- `worker::spawn(cfg, entry, worktree, run_id) -> Result<WorkerResult, CaduceusError>` — wraps Task 5.1 (timeout) + Task 5.2 (env) + Task 5.3 (result parsing). The signature is documented here because Tasks 5.1-5.3 each describe a *piece* of the spawn but the orchestrating function lives in Task 7.1.
- `queue::record_skipped(state, key)` — sets `last_error = Some("label removed mid-tick")` but does NOT increment `attempts` (Task 2.5 prose).
- `queue::mark_done(state, key, run_id)` — sets `phase = Phase::Done`, `last_run_id = Some(run_id)`, bumps `updated_at`.
- `client.last_remaining()` — accessor on `Client` (Task 2.4) that returns `Option<u16>` from the most recent response's `X-RateLimit-Remaining` header, or `None` if no response has been received yet this tick.

`run()` and `run_with_config()` share `tick()` so they cannot drift. The only difference is whether `Config::load()` is called.

**Step 2:** In `src/main.rs`, wire `clap` so that:

```rust
fn main() {
    match Command::parse() {
        Command::Run => match caduceus::run() {
            Ok(code) => std::process::exit(code as i32),
            Err(e) => { eprintln!("caduceus: {e}"); std::process::exit(1); }
        },
        Command::Status { json } => { /* see Task 7.2 */ }
    }
}
```

With `Command::Run` as the **default** subcommand (use clap's `#[command(default_subcommand = "Run")]` or handle a no-args case explicitly). The cron profile invokes `bin/caduceus` with no args, so no-args MUST run a tick.

**Step 3:** Test the loop end-to-end against a mocked GitHub API and a trivial worker that writes `worker-result.json` — this is the integration test (Task 7.5), which calls `run_with_config`.

**Step 4:** Commit: `feat(orchestrator): end-to-end tick with run + run_with_config entry points`

### Task 7.2: Persist daemon metadata (reap stats + rate-limit observations) [DONE-IN-THIS-TASK — prereq for 7.3]

**Objective:** `caduceus status` needs to report "Stale claim reaped: N (last reap: ...)" and "Rate limit reset: ... (remaining: .../5000)". The README example shows both. They must be persisted to `<state_dir>/state_meta.json` so `status` can read them after the daemon has exited. This task defines the persistence layer; Task 7.3 reads it.

**Files:**
- Modify: `src/main.rs`
- Modify: `src/status.rs`

**Step 1:** Implement `clap` subcommand. `Status` accepts `--json` for machine-readable output:

```rust
#[derive(Parser)]
enum Command {
    /// Default subcommand — runs a tick. Also fires when no args
    /// are passed (cron profile invokes the binary with no args).
    Run,

    /// Print runtime state and exit. `--json` emits JSON instead of
    /// the human-readable table.
    Status {
        #[arg(long)]
        json: bool,
    },
}
```

**Step 2:** Implement `status()` and the supporting types in `src/status.rs`. The output matches the README example line-for-line:

```rust
use serde::Serialize;
use std::path::Path;
use crate::queue;

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub daemon_version: String,             // CARGO_PKG_VERSION
    pub queue: Vec<QueueEntrySummary>,
    pub live_workers: Vec<LiveWorker>,
    pub last_reap_at: Option<String>,        // RFC3339 UTC, or None if never reaped
    pub last_reaped_count: Option<u32>,
    pub last_rate_limit_reset: Option<u64>,  // unix timestamp
    pub last_rate_limit_remaining: Option<u16>,
    pub state_path: String,                 // absolute path to state.json
}

#[derive(Debug, Serialize)]
pub struct QueueEntrySummary {
    pub key: String,
    pub phase: String,        // queued | in_progress | done | failed
    pub ticket_type: String,  // code | investigation
    pub attempts: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LiveWorker {
    pub run_id: String,
    pub elapsed_seconds: u64,
}

pub fn build_report(state_dir: &Path, daemon_version: &str) -> StatusReport {
    // 1. Read <state_dir>/state.json via StateStore::with_lock (read-only).
    //    For each entry, build a QueueEntrySummary. Sort by key for stable output.
    // 2. Read <state_dir>/state_meta.json if it exists (Task 7.3).
    // 3. Scan <state_dir>/runs/*.heartbeat to find live workers
    //    (Task 7.3 helper live_workers()).
    // 4. Assemble the StatusReport.
    unimplemented!("implemented in this task")
}

/// Print the report as a human-readable table to stdout.
pub fn print_human_readable(report: &StatusReport) {
    println!("Daemon version:     {}", report.daemon_version);
    println!("State file:        {}", report.state_path);
    println!("Queued issues:     {}", report.queue.len());
    for entry in &report.queue {
        let attempts = format!("attempts: {}", entry.attempts);
        let err = entry.last_error.as_deref()
            .map(|e| format!(", error: {e}"))
            .unwrap_or_default();
        println!("  - {} (phase: {}, {}){}", entry.key, entry.phase, attempts, err);
    }
    if let (Some(at), Some(count)) = (&report.last_reap_at, report.last_reaped_count) {
        println!("Stale claim reaped: {count} (last reap: {at})");
    }
    if let (Some(reset), Some(rem)) = (report.last_rate_limit_reset, report.last_rate_limit_remaining) {
        let iso = chrono::DateTime::from_timestamp(reset as i64, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| reset.to_string());
        println!("Rate limit reset:   {iso} (remaining: {rem}/5000)");
    }
    if !report.live_workers.is_empty() {
        println!("Live workers:       {}", report.live_workers.len());
        for w in &report.live_workers {
            println!("  - {} (elapsed: {}s)", w.run_id, w.elapsed_seconds);
        }
    }
}

pub fn print_json(report: &StatusReport) -> Result<(), CaduceusError> {
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}
```

**Step 3:** Add an integration test that creates a populated state dir and asserts the report's shape:

```rust
#[test]
fn status_report_includes_queue_live_workers_and_reap_stats() {
    let tmp = tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    // Seed state.json with two entries (one queued, one failed).
    let state_path = state_dir.join("state.json");
    std::fs::write(&state_path, serde_json::to_string(&caduceus::QueueState {
        version: 1,
        entries: vec![
            ("owner/repo#1".into(), caduceus::QueueEntry {
                key: "owner/repo#1".into(),
                phase: caduceus::Phase::Queued,
                ticket_type: caduceus::TicketType::Code,
                attempts: 2,
                last_error: None,
                last_run_id: None,
                queued_at: "2026-07-13T10:00:00Z".into(),
                updated_at: "2026-07-13T10:00:00Z".into(),
            }),
            ("owner/repo#2".into(), caduceus::QueueEntry {
                key: "owner/repo#2".into(),
                phase: caduceus::Phase::Failed,
                ticket_type: caduceus::TicketType::Investigation,
                attempts: 3,
                last_error: Some("max retries exceeded".into()),
                last_run_id: None,
                queued_at: "2026-07-13T09:00:00Z".into(),
                updated_at: "2026-07-13T10:00:00Z".into(),
            }),
        ].into_iter().collect(),
    }).unwrap()).unwrap();

    // Seed state_meta.json with reap + rate-limit stats.
    let meta_path = state_dir.join("state_meta.json");
    std::fs::write(&meta_path, serde_json::to_string(&caduceus::StateMeta {
        last_reap_at: Some("2026-07-13T09:30:00Z".into()),
        last_reaped_count: Some(1),
        last_rate_limit_reset: Some(1700000000),
        last_rate_limit_remaining: Some(4987),
    }).unwrap()).unwrap();

    // Drop a fake heartbeat into runs/.
    let runs_dir = state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    let heartbeat = runs_dir.join("01JCK9X4F7Z8R9W2K3M5N6P7Q8.heartbeat");
    std::fs::write(&heartbeat, "1700000000").unwrap();

    let report = caduceus::status::build_report(&state_dir, "0.1.0");
    assert_eq!(report.queue.len(), 2);
    assert_eq!(report.queue[0].key, "owner/repo#1");
    assert_eq!(report.queue[0].phase, "queued");
    assert_eq!(report.queue[1].phase, "failed");
    assert_eq!(report.live_workers.len(), 1);
    assert_eq!(report.live_workers[0].run_id, "01JCK9X4F7Z8R9W2K3M5N6P7Q8");
    assert_eq!(report.last_reaped_count, Some(1));
    assert_eq!(report.last_rate_limit_remaining, Some(4987));
}
```

**Step 4:** Wire `status` subcommand in `src/main.rs`:

```rust
fn main() {
    match Command::parse() {
        Command::Run => match caduceus::run() {
            Ok(code) => std::process::exit(code as i32),
            Err(e) => { eprintln!("caduceus: {e}"); std::process::exit(1); }
        },
        Command::Status { json } => {
            let state_dir = resolve_state_dir();  // env CADUCEUS_STATE_DIR or ~/.hermes/caduceus-state
            let report = caduceus::status::build_report(&state_dir, env!("CARGO_PKG_VERSION"));
            if json {
                caduceus::status::print_json(&report)
                    .unwrap_or_else(|e| { eprintln!("status: {e}"); std::process::exit(1); });
            } else {
                caduceus::status::print_human_readable(&report);
            }
            Ok(())
        }
    }
}
```

**Step 5:** Run tests, verify PASS.

**Step 6:** Commit: `feat(status): caduceus status subcommand with reap + rate-limit stats`

### Task 7.3: `caduceus status` subcommand

**Objective:** Print runtime state as JSON or human-readable. The README's "Operational Diagnostics" example (lines 281-287) requires four pieces of state: queue contents from `state.json` (Task 3.0), live workers from heartbeats (this task, Step 7), last-reap stats (Task 7.2), and the most recent rate-limit observation (Task 7.2). This task ties them all together into the human-readable output.

**Files:**
- Modify: `src/queue.rs`
- Modify: `src/worker.rs`
- Modify: `src/status.rs`
- Create: `src/meta.rs`

**Step 1:** Define the metadata struct in a new `src/meta.rs`:

```rust
use serde::{Deserialize, Serialize};

/// Daemon-level metadata persisted across ticks. Separate from
/// `state.json` (which holds per-issue queue state) because this
/// data is daemon-wide, not per-issue, and is read by `caduceus status`
/// even when the daemon has exited.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateMeta {
    /// RFC3339 UTC timestamp of the most recent reap_stale_claims call.
    /// None if the daemon has never reaped (e.g., first run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reap_at: Option<String>,

    /// Number of claims reaped in the most recent reap call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reaped_count: Option<u32>,

    /// `X-RateLimit-Reset` header (unix seconds) from the most recent
    /// GitHub response that carried one. None if no such response yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rate_limit_reset: Option<u64>,

    /// `X-RateLimit-Remaining` header value from the most recent response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rate_limit_remaining: Option<u16>,
}

impl StateMeta {
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("state_meta.json");
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),  // first run / never written
        }
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), CaduceusError> {
        let path = state_dir.join("state_meta.json");
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
```

**Step 2:** Add a `write_meta(meta: &StateMeta, state_dir: &Path)` helper to `src/queue.rs` (so Task 3.x's `with_lock` machinery can use it). Also add a `read_meta(state_dir: &Path) -> StateMeta` helper.

**Step 3:** Write failing tests:

```rust
#[test]
fn meta_round_trips_through_disk() {
    let tmp = tempdir().unwrap();
    let mut meta = caduceus::meta::StateMeta::default();
    meta.last_reap_at = Some("2026-07-13T10:00:00Z".into());
    meta.last_reaped_count = Some(2);
    meta.last_rate_limit_remaining = Some(4987);

    meta.save(tmp.path()).unwrap();
    let loaded = caduceus::meta::StateMeta::load(tmp.path());
    assert_eq!(loaded.last_reap_at.as_deref(), Some("2026-07-13T10:00:00Z"));
    assert_eq!(loaded.last_reaped_count, Some(2));
    assert_eq!(loaded.last_rate_limit_remaining, Some(4987));
    assert_eq!(loaded.last_rate_limit_reset, None);
}

#[test]
fn meta_load_returns_default_when_file_missing() {
    let tmp = tempdir().unwrap();
    let meta = caduceus::meta::StateMeta::load(tmp.path());
    assert!(meta.last_reap_at.is_none());
    assert!(meta.last_reaped_count.is_none());
    assert!(meta.last_rate_limit_remaining.is_none());
}
```

**Step 4:** Update `reap_stale_claims` (Task 3.3) to return the count of reaped claims (it already does — `removed: u32`), and update `tick()` (Task 7.1) to persist meta after reaping:

```rust
// In tick(), after the reap step:
let reaped_count = queue::reap_stale_claims(&state, cfg.stale_run_hours)?;
let mut meta = meta::StateMeta::load(&cfg.state_dir);
meta.last_reap_at = Some(chrono::Utc::now().to_rfc3339());
meta.last_reaped_count = Some(reaped_count);
meta.save(&cfg.state_dir)?;
```

**Step 5:** Update `Client::get` (Task 2.4) to record rate-limit headers from every response. Add two fields to `Client`:

```rust
pub struct Client {
    // ... existing fields ...
    pub last_rate_limit_reset: std::sync::Mutex<Option<u64>>,
    pub last_rate_limit_remaining: std::sync::Mutex<Option<u16>>,
}
```

After every successful response, parse `X-RateLimit-Reset` and `X-RateLimit-Remaining` from the response headers and store them. `last_remaining()` (already declared in tick()) reads `last_rate_limit_remaining`. Update tick() to persist these into meta after the rate-limit check:

```rust
// In tick(), after `if client.last_remaining() == Some(0) { ... }`:
let mut meta = meta::StateMeta::load(&cfg.state_dir);
if let Some(reset) = client.last_rate_limit_reset() {
    meta.last_rate_limit_reset = Some(reset);
}
if let Some(rem) = client.last_remaining() {
    meta.last_rate_limit_remaining = Some(rem);
}
meta.save(&cfg.state_dir)?;
```

(Note: `client.last_rate_limit_reset()` is a new accessor — add it to the `Client` impl in Task 2.4 alongside `last_remaining()`.)

**Step 6:** Add accessor methods to `Client`:

```rust
impl Client {
    pub fn last_remaining(&self) -> Option<u16> {
        *self.last_rate_limit_remaining.lock().unwrap()
    }
    pub fn last_rate_limit_reset(&self) -> Option<u64> {
        *self.last_rate_limit_reset.lock().unwrap()
    }
}
```

**Step 7:** Add live-worker helper to `src/status.rs`:

```rust
pub fn live_workers(state_dir: &Path) -> Vec<LiveWorker> {
    let runs_dir = state_dir.join("runs");
    let mut workers = Vec::new();
    let entries = match std::fs::read_dir(&runs_dir) {
        Ok(e) => e,
        Err(_) => return workers,  // runs/ doesn't exist → no live workers
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("heartbeat") {
            continue;
        }
        let run_id = path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".heartbeat"))  // file_stem already strips .heartbeat; this is defensive
            .unwrap_or("")
            .to_string();
        if run_id.is_empty() { continue; }
        let ts_str = std::fs::read_to_string(&path).unwrap_or_default();
        let ts: u64 = ts_str.trim().parse().unwrap_or(0);
        let elapsed = chrono::Utc::now().timestamp().max(0) as u64 - ts;
        workers.push(LiveWorker { run_id, elapsed_seconds: elapsed });
    }
    workers
}
```

**Step 8:** Run tests, verify PASS.

**Step 9:** Commit: `feat(meta): persist reap stats and rate-limit observations for status command`

### Task 7.4: Signal handling

**Objective:** Handle SIGTERM and SIGINT during a tick: if a worker is running, log the interruption, release the worktree claim, and exit cleanly. This prevents orphan worktrees when the daemon is killed mid-tick.

**Files:**
- Modify: `src/main.rs`
- Create: `tests/signal_test.rs`

**Step 1:** Write failing tests:

```rust
#[tokio::test]
async fn sigterm_during_worker_cleans_up() {
    let tmp = tempdir().unwrap();
    // ... setup ...
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("run")
        .env("CADUCEUS_CONFIG", &config_path)
        .spawn().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM); }
    let status = child.wait_with_output().unwrap();
    let store = caduceus::queue::StateStore::new(&tmp.path().join("state"));
    let entries = store.with_lock(|state| caduceus::queue::list(state)).unwrap();
    assert!(entries.iter().any(|e| e.phase == "queued"));
}

#[tokio::test]
async fn sigint_during_idle_tick_exits_cleanly() {
    // Similar test but no worker running — just verify exit code 0
}
```

**Step 2:** Implement signal handling using `tokio::signal`:

```rust
use tokio::signal;

pub async fn handle_signals(cancellation_token: tokio_util::sync::CancellationToken) {
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt()).unwrap();

    tokio::select! {
        _ = sigterm.recv() => {
            tracing::warn!("Received SIGTERM, initiating graceful shutdown");
            cancellation_token.cancel();
        }
        _ = sigint.recv() => {
            tracing::warn!("Received SIGINT, initiating graceful shutdown");
            cancellation_token.cancel();
        }
    }
}
```

**Step 3:** Integrate with the main loop via `CancellationToken`.

**Step 4:** Add `tokio-util` to `Cargo.toml`: `tokio-util = "0.7"`.

**Step 5:** Commit: `feat(signals): graceful SIGTERM/SIGINT handling with worktree cleanup`

### Task 7.5: Integration test (full pipeline)

**Objective:** Verify the entire pipeline works end-to-end: mock GitHub API → mock config → mock worker → assert PR created + issue closed.

**Files:**
- Create: `tests/integration_test.rs`

**Step 1:** Write the integration test:

```rust
#[tokio::test]
async fn full_pipeline_processes_one_issue() {
    let tmp = tempdir().unwrap();

    // 1. Set up mock GitHub API
    let mock = wiremock::MockServer::start().await;

    // Mock: GET /user/repos → return one repo
    mock.register(
        wiremock::get("/user/repos")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .body(r#"[{"full_name": "test-org/test-repo"}]"#))
    ).await;

    // Mock: GET /repos/test-org/test-repo/issues/events → return labeled event
    mock.register(
        wiremock::get("/repos/test-org/test-repo/issues/events")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"[{"type": "IssuesEvent", "action": "labeled", "label": {"name": "🤖 auto-fix"}, "issue": {"number": 1}, "repository": {"full_name": "test-org/test-repo"}}]"#))
    ).await;

    // Mock: GET /repos/test-org/test-repo/issues/1 → return issue detail
    mock.register(
        wiremock::get("/repos/test-org/test-repo/issues/1")
            .respond_with(wiremock::Response::builder()
                .status(200)
                .body(r#"{"number": 1, "title": "Test bug", "body": "Fix this bug", "labels": [{"name": "🤖 auto-fix"}]}"#))
    ).await;

    // Mock: GET /repos/test-org/test-repo/issues/1/comments → empty
    mock.register(
        wiremock::get("/repos/test-org/test-repo/issues/1/comments")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;

    // Mock: POST /repos/test-org/test-repo/pulls → create PR
    mock.register(
        wiremock::post("/repos/test-org/test-repo/pulls")
            .respond_with(wiremock::Response::builder()
                .status(201)
                .body(r#"{"html_url": "https://github.com/test-org/test-repo/pull/1"}"#))
    ).await;

    // Mock: POST /repos/test-org/test-repo/issues/1/comments (success comment)
    mock.register(
        wiremock::post("/repos/test-org/test-repo/issues/1/comments")
            .respond_with(wiremock::Response::builder().status(201).body("{}"))
    ).await;

    // Mock: PATCH /repos/test-org/test-repo/issues/1 (close)
    mock.register(
        wiremock::patch("/repos/test-org/test-repo/issues/1")
            .respond_with(wiremock::Response::builder().status(200).body("{}"))
    ).await;

    // 2. Create test config
    let config = caduceus::config::Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        state_dir: tmp.path().join("state"),
        workdir_base: tmp.path().join("repos"),
        worker_command: vec!["python3".into(), "-c".into(),
            "import json; open('worker-result.json','w').write(json.dumps({'status':'success','summary':'done','branch_name':'auto/fix-1','commit_message':'fix','pull_request_title':'fix: bug'}))".into()],
        worker_timeout_seconds: 30,
        ticket_label_code: "🤖 auto-fix".into(),
        ticket_label_investigation: "🤖 auto-fix-investigate".into(),
        poll_interval_seconds: 120,
        poll_user: "test-user".into(),
        log_path: tmp.path().join("processor.log"),
        stale_run_hours: 1,
        max_retries_per_issue: 3,
        comment_forbidden_strings: vec!["caduceus".into(), "opencode".into(), "gentle-ai".into(), "engram".into()],
        feedback_author_allowlist: vec![],
        comment_ignore_patterns: vec![],
    };

    // 3. Create a git repo in the expected location
    let repo_path = tmp.path().join("repos").join("test-org").join("test-repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    let repo = git2::Repository::init(&repo_path).unwrap();
    let sig = git2::Signature::now("test", "test@test").unwrap();
    let mut index = repo.index().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[]).unwrap();

    // 4. Run the full pipeline
    let result = caduceus::run_with_config(config).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 0);

    // 5. Verify side effects:
    //    (a) the worker's prompt file was written to the worktree root
    assert!(
        repo_path.join(".worktrees/auto/fix-1/worker-prompt.md").exists(),
        "worker-prompt.md must be written into the worktree by Task 4.4 before the worker spawns",
    );
    //    (b) the worker actually ran and committed a change to the new branch
    assert!(
        repo_path.join(".worktrees/auto/fix-1").join("worker-result.json").exists(),
        "worker-result.json must be written into the worktree by the harness before finalize",
    );
}

#[tokio::test]
async fn full_pipeline_handles_empty_queue() {
    let mock = wiremock::MockServer::start().await;
    mock.register(
        wiremock::get("/user/repos")
            .respond_with(wiremock::Response::builder().status(200).body("[]"))
    ).await;

    let tmp = tempdir().unwrap();
    let config = caduceus::config::Config {
        api_base: mock.uri(),
        github_token: Some("test-token".into()),
        state_dir: tmp.path().join("state"),
        workdir_base: tmp.path().join("repos"),
        worker_command: vec!["echo".into()],
        ..caduceus::config::Config::defaults()
    };
    let result = caduceus::run_with_config(config).await;
    assert!(result.is_ok());
}
```

**Step 2:** Add a `run_with_config(config)` entry point for testing (separate from the CLI `run()` that loads config from disk).

**Step 3:** Run tests, verify PASS.

**Step 4:** Commit: `test(integration): full pipeline end-to-end test`

---

## Phase 8: Documentation & Examples

### Task 8.1: Hermes-facing documentation and bridge contract

**Objective:** Finish the user-owned bridge template and make every public document match the root Hermes adapter implemented in Task 0.2. Follow Amendment 8.1; do not recreate the legacy plugin directory conventions.

**Files:**
- Modify: `plugin-assets/worker-bridge.py`
- Modify: `skills/caduceus/SKILL.md`
- Modify: root `__init__.py` help/status text
- Modify: `README.md`
- Create/modify: `tests/bridge_test.py`
- Create/modify: `tests/docs_contract_test.rs`
- Extend: `tests/hermes_plugin_test.py`

**RED:** Test the bridge's required JSON-label environment, exit propagation, missing/malformed input, signal behavior, and absence of heartbeat/state writes. Test public docs against the Config/env/CLI fixtures. Run a real isolated Hermes 0.18.2 install/enable/setup/cron/remove lifecycle and assert that every documented command exists and behaves as described.

**GREEN:** Update the bridge template and documentation. The template is copied by explicit setup to `$HERMES_HOME/caduceus/worker-bridge.py` only when absent. Plugin source update never overwrites the user copy; changed templates are offered as `.new` files. The namespaced skill is opt-in, the slash command is registered through `ctx`, and cron is an explicit no-agent job backed by `$HERMES_HOME/scripts/caduceus-pulse.sh`.

**REFACTOR/verify:** Search for and remove claims about `hermes plugin` (singular), profile plugin paths, automatic skill triggers, `commands/*.md` registration, cron-profile YAML, manifest defaults/binaries, or lifecycle hooks. Run bridge, docs-contract, and pinned Hermes integration tests together.

**Acceptance:** A new operator can follow README from `hermes plugins install ... --enable` through setup, doctor, cron install, status, update/rebuild, cron removal, and plugin removal without relying on an undocumented Hermes behavior. Standalone installation remains documented and functional.

**Commit:** `docs(plugin): align Hermes adapter and bridge lifecycle`

---
## Phase 9: Migration & Cutover

### Task 9.1: Migration checklist

**Objective:** Document how to migrate from a legacy Python-based issue processor to Caduceus.

**Files:**
- Create: `MIGRATION.md`

**Step 1:** Document:
- How to disable the old cron jobs
- How to map old `state.json` schemas to Caduceus's state schema (if the user's legacy system has one)
- How to backfill the `attempts` counter for already-running issues
- How to test in dry-run mode (env var `CADUCEUS_DRY_RUN=1` — finalize prints but doesn't push)

**Step 2:** Commit: `docs: migration guide from legacy issue processors`

### Task 9.2: Cutover plan

**Objective:** Step-by-step rollout plan to switch from a legacy issue processor to Caduceus in production.

**Files:**
- Append: `MIGRATION.md`

**Step 1:** Document:
- Day 0: install caduceus, configure, enable `caduceus status` monitoring, do NOT disable the legacy system
- Day 1–3: run caduceus in dry-run mode alongside the legacy system; verify queue state matches expectations
- Day 4: enable one repo at a time (start with a non-critical repo)
- Day 7: if all green, disable the legacy cron jobs

**Step 2:** Commit: `docs: cutover plan`

---

## Resolved Decisions

These were decided in the planning conversation. Each is locked in for v0.1 unless explicitly noted as deferred to a later version.

1. **State persistence format: JSON.** v0.1 uses a JSON file at `<state_dir>/state.json`, protected by `flock`. SQLite is deferred to v0.2+ — YAGNI for the current queue sizes (typically <50 entries).

2. **Deployment model: single-host only.** v0.1 assumes the daemon runs on exactly one host and the state directory is local. Multi-host deployments require a shared filesystem or moving state to a DB, and are explicitly out of scope for v0.1.

3. **Authentication: Personal Access Token (PAT) only.** v0.1 resolves the GitHub token via the hierarchy described in Task 1.2. GitHub App support is deferred to v0.2 — orthogonal to the daemon's core architecture and adds 2-3 weeks of work.

4. **PR review gating: never auto-merge.** v0.1 always opens PRs and waits for human review. Auto-merge is explicitly **out of scope** for v0.1. A v0.2 design exercise will explore whether per-label opt-in auto-merge (e.g., `🤖 auto-fix-trivial`) is viable with sufficient guardrails.

5. **Bot account comment voice: hard rule.** The daemon enforces the public-voice rule by scanning every outbound bot comment against a forbidden-string list before posting. If a match is found, the post is rejected and an error is logged. Default forbidden strings: `caduceus`, `opencode`, `gentle-ai`, `engram`. These are names of actual tools in the stack (the daemon, the default harness CLI, the default agent, and the default memory backend) — preventing them from leaking into public comments. The list is **not** hardcoded — users can extend or override it via `comment_forbidden_strings` in config (see Task 6.6).

6. **Config file location: Hermes-primary with standalone fallback.** v0.1 is a Hermes plugin, so config lives in `~/.hermes/config.yaml` under the `caduceus:` section by default. Resolution order:
   1. `$CADUCEUS_CONFIG` environment variable (path to a YAML file)
   2. `~/.hermes/config.yaml` under the `caduceus:` section (default when installed as a Hermes plugin)
   3. `~/.config/caduceus/config.yaml` (XDG-style, used when running the daemon standalone without Hermes)

   This reflects the project identity: Caduceus is a Hermes plugin first and foremost. Standalone install is supported for users who don't use Hermes, but the default and primary path is the plugin install.

7. **Why Rust for a Hermes-adjacent cron job?** The daemon runs via cron every 2 minutes, not inside the agent loop. It never needs `ctx` or Hermes tooling. Rust gives us one-shot binary deployment, robust process-group management for worker timeout enforcement, and no Python runtime dependency for the daemon. The Hermes plugin (Python) handles everything the agent touches — skills, commands, cron profile, config integration. The boundary is clean: Python where the agent reaches, Rust where the system runs.

## v0.2+ Considerations (explicitly deferred)

These were discussed and intentionally deferred. Tracking them here so they're not forgotten:

- **SQLite-backed state.** When queue sizes routinely exceed ~1,000 entries, or when we want rich query surfaces (e.g., "show me all repos that failed 3+ times this week"), migrate from JSON to SQLite. Migration tooling should preserve existing claim files.
- **Multi-host deployment.** If high availability becomes a requirement, the cleanest path is to migrate to a shared DB (SQLite via Litestream or a managed PostgreSQL) rather than build consensus into the daemon itself.
- **GitHub App authentication.** Adds higher rate limits, clearer bot identity, and fine-grained permissions. Requires app registration, JWT signing, and installation-token refresh logic.
- **Auto-merge for trusted labels.** A future version may allow specific labels (e.g., `🤖 auto-fix-trivial`) to be auto-merge-eligible after CI passes. Requires careful scoping to prevent bug-class PRs from being silently merged.
- **Plugin-level status surfaces.** The Hermes plugin can expose Caduceus state in the TUI / Telegram / dashboard. Currently out of scope — users query via `caduceus status`.

---

## Risks & Tradeoffs

| Risk | Mitigation |
|---|---|
| GitHub API breaking changes | Pin to documented REST API version (`X-GitHub-Api-Version: 2022-11-28`) |
| Worker hangs after timeout fires (SIGKILL'd but children still alive) | Use `prctl(PR_SET_PDEATHSIG)` or process group; on Linux, send SIGKILL to the entire process group |
| Token leaks via env to child | Explicitly filter known credential env vars; document a "deny by default" policy |
| Long-running worker DOSes the queue | Concurrency cap = 1 (single tick); next tick waits until this one finishes |
| Retry storm on persistent failure | `max_retries_per_issue` cap (default 3) |
| ETag cache corruption | Validate ETag format; on parse failure, refetch unconditionally |
|| Worktree leaks (not cleaned up on crash) | Stale-claim reaper removes the claim; `caduceus worktree-gc` subcommand prunes orphaned `.worktrees/` dirs older than 7 days |
|| Daemon killed mid-tick (SIGTERM/SIGINT) | Signal handler releases claim + cleans up worktree before exit (Task 7.4) |
|| Unintentional tool name leak in bot comments | Hard voice-rule check on every outbound comment (Task 6.6) |
|| Worker command typo in config | Startup validation checks PATH before entering main loop (Task 1.6) |
|| Config file not found | Resolution chain has three fallback paths + explicit error message |
