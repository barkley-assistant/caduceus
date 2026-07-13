# Caduceus v0.1 — Normative Contracts

> This is the sole normative cross-task contract for implementation. Task packets may narrow scope but may not override this file. The reviewed monolith is retained only as a non-authoritative audit source.

## Goal and scope

Caduceus is a Unix single-host, one-shot Rust daemon shipped as a Hermes plugin. Each invocation polls GitHub for open issues carrying one of two configured trigger labels, atomically queues at most one unit of work, provisions an isolated git worktree, runs a user-editable harness bridge under a hard process-tree timeout, and finalizes a successful code result as a commit, push, pull request, and issue close. Investigation results are posted as findings without a code commit or PR. Linux is the tier-1 release platform; macOS is supported through the same Unix supervisor/session contract.

The daemon owns GitHub credentials, polling, state, claims, worktrees, prompts, environment construction, process groups, transcripts, heartbeats, git operations, public-text validation, retries, and status metadata. The bridge owns only translation from `CADUCEUS_*` inputs to a harness command and propagation of the harness exit code.

## Non-negotiable v0.1 invariants

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

## Toolchain and dependencies

- Rust 2021, MSRV 1.75
- Runtime: `tokio`, `tokio-util`, `reqwest`, `serde`, `serde_json`, `serde_yaml`, `clap`, `tracing`, `tracing-subscriber`, `tracing-appender`, `thiserror`, `fs2`, `ulid`, `chrono`, `regex`, `which`, `shellexpand`, `sha2`, `hex`, `filetime`, `walkdir`, `libc`
- Git implementation: shell out to the installed `git` executable. This avoids libgit2 credential divergence and uses the operator's existing SSH agent or credential helper. Every invocation uses argument arrays, never a shell string.
- Dev: `tempfile`, `wiremock`, `assert_fs`, `predicates`, `serial_test`
- Python bridge tests: `pytest`

Commit `Cargo.lock` and use `--locked` for CI, plugin, and release builds. The dependency resolver must pass the release suite on Rust 1.75; upgrading a crate in a way that raises MSRV is a documented compatibility change, not an incidental lockfile refresh.

## Canonical public contracts

### Configuration

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

### Comment filtering and public-voice matching

`comment_ignore_patterns` is an ordered list of Rust `regex` expressions. Each expression is compiled during configuration validation; an invalid expression is a configuration error. Matching uses the regex crate's default case-sensitive, unanchored `is_match` semantics against the complete GitHub comment-author login. A configured expression may opt into case-insensitivity with its own `(?i)` flag. If any expression matches, that author is excluded from both `issue_comments` and `trusted_comments`. Explicit configuration replaces the default bot patterns.

`comment_forbidden_strings` is an ordered list of non-empty terms. Every outbound GitHub comment, pull-request title, and pull-request body is rejected before its corresponding API mutation when any term matches by case-insensitive Unicode substring. Explicit configuration replaces the defaults. This outbound public-voice rule is distinct from inbound comment filtering.

### Issue identity and queue schema

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

### Polling contract

The daemon does not consume GitHub's heterogeneous Events API. It discovers repositories with paginated `GET /user/repos?per_page=100&sort=full_name` unless `watched_repos` is configured, then performs one paginated open-issue query per URL-encoded trigger label: `GET /repos/{slug}/issues?state=open&labels={label}&per_page=100&sort=updated&direction=desc`. Results are merged by case-insensitive issue key. Pull-request objects are excluded by the presence of `pull_request`. Trigger labels are still verified from each returned object's label array rather than trusting the query alone. An issue present in both mutually exclusive result sets is reported as ambiguous and is not enqueued until a user removes one.

Every GET page has a persisted ETag entry in `<state_dir>/cache/http.json`. A 304 reuses the last successfully parsed body stored with that ETag. Cache writes are atomic. Invalid cache JSON or an invalid ETag drops only the affected cache entry and refetches unconditionally. The first tick processes current labeled issues; there is no historical event replay.

All requests set `User-Agent: caduceus/<version>`, `Accept: application/vnd.github+json`, and `X-GitHub-Api-Version: 2022-11-28`. All non-2xx/304 statuses become typed errors. `Link` pagination is followed within a configurable hard maximum of 20 pages per endpoint; exceeding it is an error rather than silent truncation.

### Worker environment and result

The child receives exactly these Caduceus variables:

- `CADUCEUS_ISSUE_NUMBER`, `CADUCEUS_ISSUE_TITLE`, `CADUCEUS_ISSUE_BODY`, `CADUCEUS_ISSUE_REPO`
- `CADUCEUS_ISSUE_LABELS_JSON` (JSON array; the comma-separated variable is removed)
- `CADUCEUS_WORKTREE_PATH`, `CADUCEUS_RUN_ID`, `CADUCEUS_CONTEXT_JSON`
- `CADUCEUS_BRANCH_NAME` (daemon-owned expected branch)

The inherited allowlist defaults to `PATH`, `HOME`, `USER`, `SHELL`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, plus variables matching `OPENAI_*`, `ANTHROPIC_*`, `OPENROUTER_*`, and `OPENCODE_*`. GitHub credential names are denied even if users add them to the allowlist. Startup logs variable names and redacted presence only, never values. Because the worker normally runs as the daemon's OS user, it may still be able to read that user's credential files; operators requiring a hostile-worker security boundary must run the bridge in a separately configured container/user sandbox.

Allowlist syntax is an exact variable name or one terminal `*` prefix pattern such as `OPENAI_*`. Any other wildcard placement, empty entry, `=`, NUL, or nonportable variable name is a configuration error.

### Filesystem permissions

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

### Finalization contract

The daemon creates `automation/issue-<number>-<run-id-lowercase>` before worker launch and exports it. Code success requires at least one tracked or untracked change other than daemon control files (`worker-prompt.md`, `worker-result.json`, dry-run reports). The daemon excludes those files from commits, commits all remaining changes, pushes `HEAD:refs/heads/<branch>`, finds or creates an open PR for that head/base, posts a completion comment if not already present, and closes the issue if still open. Each step treats an already-achieved state as success.

Investigation success posts a voice-checked findings comment derived from `summary` and leaves the issue open with the trigger label removed. It performs no commit, push, or PR creation.

Dry-run performs polling, claim, issue fetch, prompt creation, worker execution, result validation, and change inspection. It performs no commit, push, comment, label mutation, PR, or issue close. It writes `<state_dir>/runs/<run_id>.dry-run.md` before teardown.

### State metadata and status

`<state_dir>/state_meta.json` contains schema version, tick start/finish/outcome, last HTTP status, next allowed poll time, reap time/count, rate-limit limit/remaining/reset, and last error. It uses the same atomic writer as queue state.

`caduceus status [--json]` loads configuration through the normal resolution chain, then reads that config's `state_dir`. It reports version, last tick timestamps/outcome, currently running worker and transcript, counts by queue phase, FIFO next head, recent errors, reap stats, and rate-limit data. Missing state yields a distinct nonzero diagnostic; corrupt state yields a nonzero diagnostic preserving the file. Heartbeats older than 90 seconds are stale, not live.

### CLI contract

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

### Hermes plugin compatibility contract

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

## Error contract

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

## Resolved decisions

1. **JSON state on one local host.** SQLite and multi-host state remain deferred, but JSON writes are atomic and recoverable.
2. **PAT/API token authentication for v0.1.** GitHub App authentication is deferred. Git pushes use normal git credential helpers or SSH, not API-token injection.
3. **Never auto-merge.** Code tickets open PRs for human review.
4. **Public-voice enforcement is mandatory.** It covers every public string, including worker-derived PR content.
5. **Hermes-primary configuration with standalone fallback.** Explicit plugin setup seeds a user-owned bridge under `HERMES_HOME`; standalone users configure `worker_command` explicitly.
6. **Rust/Python boundary remains fixed.** Python translates harness invocation only; Rust owns all durable/runtime state.
7. **One-shot cron model remains fixed.** Cross-invocation ETags, cadence metadata, and a full-tick lock make it safe.
8. **Daemon-owned branches.** Worker-selected refs are removed from the stable bridge contract.
9. **Investigation is a comment workflow, not a PR workflow.** It posts findings, removes its trigger label, and leaves the issue open.

## Contract revision control

`CONTRACTS.md` is sealed by `contracts_sha256` in `task-manifest.json`. A checksum mismatch is a safety stop, not permission for an implementation agent to update the digest. An agent that finds a genuine contradiction records it as a bounded blocker with evidence and does not edit the contract, manifest digest, or archive.

Only an explicitly authorized reviewer may make a contract revision. The reviewer records the rationale, affected task IDs, and required re-verification in `CONTRACT_REVISIONS.md`; updates every affected contract, task packet, phase gate, manifest field, and public documentation together; updates `contracts_sha256`; then runs the plan validator. Completed work affected by the revision must be re-verified before dependent work proceeds. `archive/full-reviewed-plan.md` is immutable and its pinned digest must never be refreshed.

## Explicit v0.2+ deferrals

- SQLite/PostgreSQL state and multi-host high availability.
- GitHub App authentication.
- Auto-merge and CI/review gating.
- Native Hermes dashboard widgets. The shipped `/caduceus-status` chat command is v0.1 scope and is not deferred.
- Parallel workers. v0.1 intentionally processes one issue per host-wide tick.
- Automatic reset of terminal failed entries. v0.1 recovery tooling is explicit and auditable.

No deferred item is required to uphold a v0.1 promise. Process-group termination, durable ETags, crash-safe JSON, idempotent finalization, and chat status are explicitly not deferred.

## Risk register

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

## Definition of done

The plan is implemented only when a fresh Hermes install can run after explicit setup with its seeded user-owned bridge, no-argument cron ticks are silent on success, a standalone install fails with a precise missing-worker instruction, every README status field is backed by persisted data, all worker descendants die on timeout/shutdown, retries and claims make progress without waiting for stale reaping, corrupt state is preserved, and the complete release gate in Task 9.2 passes.
