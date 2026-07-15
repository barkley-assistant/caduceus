# Caduceus

> **Hermes Agent plugin for self-hosted, event-driven GitHub issue automation.**

Caduceus is a Rust daemon that polls GitHub for labeled issues, runs a user-configured AI harness against them in isolated worktrees, enforces hard timeouts, and finalizes the result as a branch + push + PR. It is shipped as a **Hermes plugin** so it integrates with Hermes commands, skills, and cron delivery. Hermes installs the source adapter; one explicit `hermes caduceus setup` step builds the Rust binary safely.

v0.1 targets Unix hosts, with Linux as the tier-1 platform. Worktrees isolate ordinary repository edits but are not an OS sandbox; use a separate user or container when the harness itself is untrusted.

```
                    [ GitHub REST API ]
                             │
                      Outbound Pull Only
                             ▼
              ┌─────────────────────────────┐
              │       CADUCEUS DAEMON       │
              │          (Rust Core)        │
              │  - ETag-aware 304 Polling   │
              │  - POSIX flock Queue State  │
              │  - Isolated Git Worktrees   │
              │  - Hard Worker Timeout      │
              └──────────────┬──────────────┘
                             │
            Spawns Process   │ Sanitized Env Vars
            (Isolated cwd)   │ (No GitHub Credentials)
                             ▼
              ┌─────────────────────────────┐
              │      PLUGGABLE BRIDGE       │
              │      (Python, user-edit)    │  <-- Translate CADUCEUS_* env vars
              └──────────────┬──────────────┘    to your harness's CLI
                             │
              ┌──────────────┴──────────────┐
              ▼                             ▼
       ┌──────────────┐            ┌────────────────┐
       │   OpenCode   │            │ pi · codex ·   │
       │ + orchestr.  │            │ claude-code ·  │
       │              │            │ your custom... │
       └──────────────┘            └────────────────┘
```

By explicitly decoupling deterministic infrastructure (Rust) from non-deterministic AI resolution (Python/TypeScript/Bash), Caduceus keeps worker scripts isolated from managing raw GitHub API state, credentials, and queue races. Workers still use whatever outbound model/provider access their configured harness requires.

Caduceus is **harness-agnostic**. Whether your worker invokes OpenCode, pi, codex, claude-code, or a custom agent, the contract is the same: the bridge reads `CADUCEUS_*` env vars, runs the harness, and the harness writes a `worker-result.json` file describing what it did. Caduceus never assumes any specific harness, workflow, or methodology.

## Installation (Hermes plugin)

```bash
# Hermes Agent v0.18.2 or newer
hermes plugins install barkley-assistant/caduceus --enable

# Build/install the Rust binary and seed the user-owned bridge
hermes caduceus setup

# Configure (in your ~/.hermes/config.yaml under caduceus: section)
# See "Configuration" below

# Install/reconcile the two-minute no-agent cron job
hermes caduceus cron-install
```

Hermes installs plugin source but does not run manifest build/hook steps. Caduceus therefore exposes an explicit, idempotent `setup` command. After `hermes plugins update caduceus`, rerun `hermes caduceus setup`. Before removal, run `hermes caduceus cron-remove`, then `hermes plugins remove caduceus`; daemon state and the user-owned bridge are preserved.

## Installation (standalone — no Hermes)

If you don't use Hermes, you can install the daemon binary directly:

```bash
git clone https://github.com/barkley-assistant/caduceus.git
cd caduceus
cargo build --release --locked
cp target/release/caduceus ~/.local/bin/caduceus

# Then create your config at ~/.config/caduceus/config.yaml
# See "Configuration" below
```

You lose the plugin's skill/command/Telegram integration, but the daemon works identically. Config in `~/.config/caduceus/config.yaml` is the recommended location when not using Hermes.

## Why Caduceus Exists

Bundling GitHub orchestration and AI-driven code generation in a single process is a debugging nightmare: a stalled LLM API call can hold the queue hostage, config files affect multiple subsystems at once, and a single hung subprocess is invisible until you `strace` it.

Caduceus fixes this by enforcing a strict boundary: the daemon owns **process lifecycle, IO, and atomicity guarantees**; the worker owns **code understanding and edits**. The worker is replaceable (Python script, OpenCode invocation, anything that respects a process contract), and the daemon is observable (`caduceus status` exposes runtime state at any moment).

## Key Features

- **Zero Inbound Networking:** Operates completely on an outbound pull model. No webhooks or exposed ports are required, making it suitable for hosts with tightly restricted inbound access.
- **Blazing Fast & Low Footprint:** Utilizes Rust's native `reqwest` client with full ETag-aware caching. If there are no new updates, it exits in milliseconds with zero memory overhead.
- **Hard Worker Timeout:** The daemon enforces `worker_timeout_seconds` on every worker invocation. If a harness hangs, an internal supervisor terminates the worker session/descendants, flushes the bounded transcript, and releases the claim.
- **Crash-Safe Single-Host State:** Queue writes use file locks, atomic replacement, and per-issue exclusive claims. A host-wide tick lock prevents overlapping cron runs from processing in parallel.
- **Isolated Git Workspace:** Every triggered issue is provisioned in a temporary git worktree, keeping ordinary worker edits out of the primary checkout. This is workspace isolation, not an OS security sandbox; run untrusted harnesses inside your own container or sandbox.
- **Daemon Credential Non-Propagation:** Caduceus clears the worker environment and never injects its resolved GitHub token into the bridge or harness. A same-user process can still read files that Unix permissions allow; use a separate user/container for hostile-worker isolation.
- **Bounded Retry Budget:** Each issue has a per-issue retry counter. After N failures, it transitions to a `failed` state and stops being claimed automatically — preventing infinite crash loops.

## The Single Worker Contract

Caduceus has exactly one worker path: a **harness bridge script** that you (the user) configure to invoke whichever AI harness you want — OpenCode today, pi or Codex or anything else tomorrow. The bridge receives `CADUCEUS_*` env vars from Caduceus, translates them into the harness's CLI surface, runs the harness, and exits with its exit code. Caduceus doesn't know or care which harness is on the other end.

For investigation tickets (`🤖 auto-fix-investigate` label), the same bridge script runs — its behavior is driven by the labels passed through `CADUCEUS_ISSUE_LABELS_JSON`. A successful investigation posts findings, removes the investigation trigger label, and leaves the issue open; it does not create a commit or PR.

The bridge is the **stable API** for harness integration. Setup seeds a reference Python bridge at `$HERMES_HOME/caduceus/worker-bridge.py` (normally `~/.hermes/caduceus/worker-bridge.py`) that invokes OpenCode with the `gentle-orchestrator` agent, but **the bridge is a starting point, not a constraint**. To switch harnesses:

1. Edit the user-owned `worker-bridge.py`; plugin source updates do not overwrite it
2. Edit the `invoke_harness()` function to call your preferred harness's CLI (pi, codex, claude-code, etc.)
3. Caduceus keeps working unchanged

The bridge owns the translation between Caduceus's env-var contract and your harness's CLI surface. Everything else — worktree provisioning, timeouts, queue management, finalize — stays in Caduceus.

### The Worker: Harness Bridge Pattern

Caduceus spawns a **bridge script** (typically Python) that you configure to call whichever harness you want. We ship a reference bridge (`worker-bridge.py`) that invokes OpenCode with the `gentle-orchestrator` agent. You fork it to plug in a different harness — pi, codex, claude-code, or anything else.

**Out of the box (OpenCode):**

```python
# worker-bridge.py — reference implementation
def invoke_harness(worktree, prompt_file, run_id, labels, branch_name):
    return subprocess.run([
        "opencode", "run",
        "--agent", "gentle-orchestrator",
        "-f", str(prompt_file),
        "--", "Run the workflow per the attached prompt file."
    ], cwd=worktree).returncode
```

**Plugging in a different harness** (e.g., pi):

```python
# your-copy/worker-bridge.py — your edits
def invoke_harness(worktree, prompt_file, run_id, labels, branch_name):
    return subprocess.run([
        "pi", "--workdir", str(worktree),
        "--prompt-file", str(prompt_file),
        "--run-id", run_id,
    ], cwd=worktree).returncode
```

The bridge does only two things:

1. **Translate** `CADUCEUS_*` env vars into the harness's CLI flags
2. **Propagate** the harness's exit code so Caduceus's `worker_timeout_seconds` and transcript capture work correctly

Everything else — worktree provisioning, polling, atomic claims, finalize, comment posting — stays in Caduceus. The harness can be replaced without touching the daemon.

For investigation tickets (`🤖 auto-fix-investigate` label), the same bridge runs. The harness reads `CADUCEUS_ISSUE_LABELS_JSON` and behaves accordingly. The bridge does **not** need to fork its behavior — it just passes the labels through.

### Context Injection (Environment Variables)

Caduceus injects the absolute context of the issue directly into the child process environment:

| Variable | Purpose |
|---|---|
| `CADUCEUS_ISSUE_NUMBER` | The numeric ID of the active GitHub issue. |
| `CADUCEUS_ISSUE_TITLE` | The raw string title of the issue. |
| `CADUCEUS_ISSUE_BODY` | The core markdown body text of the issue. |
| `CADUCEUS_ISSUE_REPO` | Full `owner/repo` slug. |
| `CADUCEUS_ISSUE_LABELS_JSON` | JSON array of current label names (safe for labels containing commas). |
| `CADUCEUS_WORKTREE_PATH` | Absolute path to the isolated directory where the worker is executing. |
| `CADUCEUS_RUN_ID` | Unique ULID/UUID identifying this run; used as the transcript log filename. |
| `CADUCEUS_CONTEXT_JSON` | Structured JSON compiling historical timeline, trusted edits, and allowed user comment threads for advanced multi-turn agent context tracking. |
| `CADUCEUS_BRANCH_NAME` | Daemon-owned branch name for this run. Workers may read it but must not create or rename branches. |

Caduceus explicitly **does not** propagate its `GITHUB_TOKEN`, `GH_TOKEN`, `CADUCEUS_GITHUB_TOKEN`, or `AUTO_ISSUE_GITHUB_TOKEN` environment credentials to the worker. This is an environment contract, not an OS sandbox: a bridge running as your user can access files readable by that user. If the worker needs GitHub access, configure a separate least-privilege bot credential and isolate it from the daemon credential.

### Worker Resolution Expectation

Your script simply reads the environment variables, alters code files directly inside the active directory, and indicates its outcome via its system exit state:

- **Exit Code 0 (Success):** The script successfully resolved the issue. It must write a result payload to `worker-result.json` in the worktree root. For code tickets, Caduceus commits the changes to its daemon-owned branch, pushes, opens or reuses a Pull Request, and closes the origin ticket. For investigation tickets, it posts findings without a commit or PR.
- **Non-Zero Exit Code (Failure/Abstain):** The script failed to find a valid solution or errored out. Caduceus catches the failure cleanly, captures the full stdout/stderr execution transcript to a dedicated runner log, tears down the worktree safely, and unlocks the queue.

### Required `worker-result.json` Schema (For Exit 0):

```json
{
  "status": "success",
  "summary": "Detailed markdown text describing what was done. Becomes the description of the Pull Request.",
  "commit_message": "fix(component): resolve the issue",
  "pull_request_title": "fix(component): automatic mitigation of issue"
}
```

All fields shown above are required. The schema is deliberately minimal and harness-agnostic. Caduceus owns and validates the branch; the worker supplies only the summary and proposed commit/PR text. Investigation workers use the same stable schema, although commit and PR fields are ignored.

If your harness produces additional artifacts (specs, designs, test outputs, logs), you can include them under an optional `artifacts` object. Caduceus safely renders them in the code-ticket PR description or investigation findings comment but does not interpret their contents:

```json
{
  "status": "success",
  "summary": "...",
  "commit_message": "...",
  "pull_request_title": "...",
  "artifacts": {
    "spec_path": "openspec/changes/issue-42/spec.md",
    "design_path": "openspec/changes/issue-42/design.md",
    "test_output": "frontend/coverage/lcov.info"
  }
}
```

## Plugin Capabilities

When installed, enabled, and set up as a Hermes plugin, Caduceus adds:

- **`caduceus:caduceus` skill** — An opt-in, namespaced plugin skill for configuration and diagnostics. Hermes plugin skills are not automatically injected from conversational trigger phrases.
- **`/caduceus-status` command** — Quick status check from your chat surface (TUI or Telegram). Shows queue contents, last-run timestamps, current retry budget.
- **`hermes caduceus` CLI** — Explicit `setup`, `doctor`, `status`, `cron-install`, and `cron-remove` lifecycle commands.
- **A no-agent cron job** — `cron-install` places a Bash wrapper under `$HERMES_HOME/scripts/` and reconciles one two-minute `caduceus` job. The Hermes gateway or a configured managed cron provider must be running for it to fire.

The repository root is the Hermes plugin root. Hermes discovers `plugin.yaml` and `__init__.py`; it does not consume the historical `plugin/` subdirectory scaffolding (the older pre-0.18 plugin shape is removed).

## Quick Start (Plugin path)

Once installed and set up using the commands above:

### 1. Configure

Add to your `~/.hermes/config.yaml`:

```yaml
caduceus:
  poll_interval_seconds: 120
  state_dir: "~/.hermes/caduceus-state"
  log_path: "~/.hermes/caduceus-state/processor.log"
  workdir_base: "~/projects"

  # Optional. Empty means discover accessible, non-archived repositories
  # through GitHub's /user/repos endpoint.
  watched_repos:
    - "your-org/your-repo"

  # The pluggable execution worker definition.
  # After plugin setup, the default points at the user-owned bridge under
  # $HERMES_HOME/caduceus/, which initially invokes
  # OpenCode with the gentle-orchestrator agent. Edit the bridge (or
  # write your own) to plug in pi, codex, claude-code, or any other
  # harness — Caduceus doesn't care which one is on the other end.
  worker_command:
    - "python3"
    - "/path/to/your/worker-bridge.py"
  worker_timeout_seconds: 3600
  http_timeout_seconds: 60
  git_timeout_seconds: 300
  run_retention_days: 30

  # Security Trust Tiers
  feedback_author_allowlist:
    - "trusted-maintainer-username"
    - "id:12345678"   # Numeric GitHub user ID — protects against rename spoofing
  # Each entry is a separate regex pattern. The defaults filter standard
  # bots so their comments don't pollute the worker context. Substring
  # match against the comment author login (e.g. `dependabot[bot]`).
  comment_ignore_patterns:
    - "dependabot\\[bot\\]"
    - "github-actions\\[bot\\]"

  # Bounded retry budget — after 3 failures, the issue is shelved
  max_retries_per_issue: 3
  retry_backoff_seconds: 300

  # Activation Labels
  ticket_label_code: "🤖 auto-fix"
  ticket_label_investigation: "🤖 auto-fix-investigate"
```

> **Defaults are conservative — review before production.** Restrict `watched_repos` explicitly when the authenticated account can access repositories that should not be automated.

**About the allowlist:** Each entry is either a GitHub login or `id:<numeric>` where `<numeric>` is the user's GitHub numeric user ID (visible via `https://api.github.com/users/<login>` in the `id` field). Numeric IDs are recommended for security-sensitive contexts because they survive username renames — a user who renames their account to bypass an allowlist still matches the numeric ID. The daemon extracts the numeric ID from each comment's `user.id` field at fetch time; no extra API call is required.

**About `comment_ignore_patterns`:** A list of regex patterns matched against each comment author's login (substring match, case-sensitive). The default patterns filter standard bot accounts (`dependabot[bot]`, `github-actions[bot]`) so their automated comments don't pollute the worker context. The list **replaces** the defaults if you set any values — to keep the defaults, set the list back to both patterns explicitly.

### 2. Prepare local repository clones

Caduceus v0.1 creates worktrees from existing clones; it does not clone missing repositories automatically. Place each watched repository at `<workdir_base>/<owner>/<repo>` and configure its `origin` credential helper or SSH key for noninteractive fetch/push:

```bash
mkdir -p ~/projects/OWNER
git clone git@github.com:OWNER/REPO.git ~/projects/OWNER/REPO
git -C ~/projects/OWNER/REPO remote set-head origin --auto
```

The `origin` owner/repository and host must match the watched GitHub repository. The primary checkout must be clean before a run starts.

### 3. Create Labels In Your Target Repositories

```bash
gh label create "🤖 auto-fix" --repo OWNER/REPO --color "7C3AED" --description "Triggers Caduceus code automation pipeline"
gh label create "🤖 auto-fix-investigate" --repo OWNER/REPO --color "7C3AED" --description "Triggers Caduceus analysis summary"
```

### 4. Install the cron job

```bash
hermes caduceus cron-install
```

Caduceus is now running silently every 2 minutes.

To preview safely, set `CADUCEUS_DRY_RUN=1`. A successful preview is recorded as `previewed`; no commit or GitHub mutation occurs. When dry-run is disabled, a still-labeled preview automatically returns to the real queue. Failed/skipped entries are reset only through `caduceus queue reset OWNER/REPO#N [--dry-run]`; do not edit state files directly.

## Operational Diagnostics

### `caduceus status` (CLI)

Inspect the daemon's runtime state without grepping logs:

```bash
$ caduceus status
Daemon version:     0.1.0
Last run started:   2026-07-12T21:50:00Z
Last run finished:  2026-07-12T21:50:03Z (304 Not Modified — idle tick)
Currently running:  no
Queue phases:       queued=2 in_progress=0 previewed=0 failed=1 skipped=1 done=12
Next head:          your-org/your-repo#338 (attempts: 2)
Recent errors:
  - your-org/your-repo#336 (failed: worker timeout, attempts: 3)
  - your-org/your-repo#334 (skipped: label removed before work, attempts: 0)
Stale claim reaped: 1 (last reap: 2026-07-12T21:30:00Z)
Rate limit reset:   2026-07-12T22:14:23Z (remaining: 4987/5000)
```

### `/caduceus-status` (in chat)

From your Hermes TUI or Telegram:

```
/caduceus-status
```

Same information as the CLI, but formatted for your chat surface and routed through your normal notification channel.

### Session Transcripts

Every worker invocation records its absolute execution lifespan directly to disk. If an agent goes off the rails or throws an error, navigate to:

```bash
tail -n 100 ~/.hermes/caduceus-state/runs/<run-id>.log
```

These log dumps serve as your definitive post-mortem trail for diagnosing non-deterministic script outputs or edge-case pipeline crashes.

### Token Resolution Flow

Caduceus automatically determines authentication authorization via a structural resolution hierarchy. It checks across these vectors in decreasing order of preference:

1. Explicit `github_token` field configured inside your local `~/.hermes/config.yaml`.
2. `CADUCEUS_GITHUB_TOKEN` system environment variable.
3. `GITHUB_TOKEN` system environment variable.
4. Active fallback query to local installation state variables via `gh auth token`.

For private repositories, a fine-grained PAT needs repository **Metadata: read**, **Contents: read/write**, **Issues: read/write**, and **Pull requests: read/write**. The API token and the git remote credential are separate concerns: configure the repository's SSH key or git credential helper for noninteractive fetch/push.

### Rate Limiting

If GitHub responds with `429` or `X-RateLimit-Remaining: 0`, Caduceus will:

- Log the rate-limit reset time
- Exit cleanly (cron will retry next tick)
- **Not** attempt any further API calls until `X-RateLimit-Reset`

### Worker Environment

Caduceus clears and rebuilds the worker environment. It explicitly **does not** pass daemon GitHub credential variables. The worker is responsible for ensuring its own runtime — for example, an `opencode`-based worker must have:

- The `opencode` binary on `PATH`
- A working provider/model configuration in `~/.config/opencode/opencode.json` (or via `OPENCODE_*` env vars)
- Any model-specific env vars (`OPENROUTER_API_KEY`, `OPENCODE_API_KEY`, etc.)

Caduceus logs the worker's resolved environment at startup (with secrets redacted) so you can debug `command not found` / provider-not-found issues.

## Configuration Variables Reference

| Key | Default | Purpose |
|---|---|---|
| `poll_interval_seconds` | `120` | Minimum execution cadence (will respect GitHub's `X-Poll-Interval` if longer). |
| `state_dir` | `~/.hermes/caduceus-state` | Location on disk where global states, atomicity lock-tokens, and running queues are managed. |
| `log_path` | `<state_dir>/processor.log` | Structured daemon log file. |
| `watched_repos` | `[]` | Explicit `owner/repo` list. Empty discovers accessible, non-archived repositories via `/user/repos`. |
| `stale_run_hours` | `1` | Automatic crash-recovery threshold. Active issue claims older than this are reaped on next tick. |
| `worker_command` | `$HERMES_HOME/caduceus/worker-bridge.py` after plugin setup | Exact argument array used to invoke the bridge. Standalone installs must set it explicitly. |
| `worker_timeout_seconds` | `3600` | Hard timeout cap enforced by Caduceus before forcefully terminating a worker. |
| `http_timeout_seconds` | `60` | Total timeout for each GitHub HTTP request; connect timeout is 10 seconds. |
| `git_timeout_seconds` | `300` | Timeout for fetch, push, and other git subprocesses; interactive credential prompts are disabled. |
| `transcript_max_bytes` | `10485760` | Maximum transcript bytes retained per run; output is still drained after truncation. |
| `run_retention_days` | `30` | Retain inactive transcripts/results/reports for this many days; active and resumable runs are exempt. |
| `workdir_base` | `~/projects` | Directory path where target repository clones are stored and worktrees are dynamically spun out. |
| `feedback_author_allowlist` | `()` | Logins or numeric user IDs whose comments/inputs are granted Trusted rank classification. |
| `comment_ignore_patterns` | standard bots | A regex list of users whose commentary additions should be explicitly skipped. |
| `max_retries_per_issue` | `3` | Worker-attributable failed attempts allowed. The third worker failure transitions the issue to `failed`. |
| `retry_backoff_seconds` | `300` | Delay before retrying a worker-attributable failure. Infrastructure failures do not consume the worker retry budget. |
| `worker_env_allowlist` | runtime/provider defaults | Exact names or supported prefixes preserved after the worker environment is cleared. GitHub credentials are always denied. |
| `ticket_label_code` | `🤖 auto-fix` | GitHub issue label used to queue structural pull-request code generation workflows. |
| `ticket_label_investigation` | `🤖 auto-fix-investigate` | GitHub issue label used to generate automated research descriptions without code PRs. |
| `comment_forbidden_strings` | see below | String list enforced on every bot comment (hard rule). See the "Public Voice Rule" section. |
| `github_token` | unset | Optional explicit GitHub API token; environment and `gh auth token` fallbacks are preferred for avoiding plaintext config secrets. |
| `api_base` | `https://api.github.com` | GitHub REST API base URL; primarily useful for GitHub Enterprise and tests. |
| `dry_run` | `false` | Skip all git/GitHub mutations after worker validation. `CADUCEUS_DRY_RUN` overrides YAML. |

## Public Voice Rule (Hard Enforcement)

The daemon refuses to publish any bot comment, Pull Request title, or Pull Request body containing the strings in `comment_forbidden_strings`. This is a **hard rule** — validation happens before the corresponding GitHub mutation, and a forbidden value is never posted.

**Default forbidden strings:** `caduceus`, `opencode`, `gentle-ai`, `engram` (all matched case-insensitive substring). These are the names of actual tools in the Caduceus stack — the daemon refuses to mention them so users don't leak their internal tooling into public issue/PR comments.

**Why substring matching?** It catches every variant (`Caduceus`, `CADUCEUS`, `caduceus-bot`) without writing complex parsers. The cost is occasional false positives — e.g., the string `"opencode"` could match inside a longer word that happens to contain those letters. If your comment genuinely needs to mention such a string, override the list.

**Override the default list** by setting `comment_forbidden_strings` in your config. **Explicit values replace the defaults entirely** — they do not merge. This keeps the rule strict: a user who lists one forbidden string is signaling they've thought about the rule and want that exact list.

```yaml
caduceus:
  # Example: keep defaults except drop "engram" if you don't use it
  comment_forbidden_strings:
    - "caduceus"
    - "opencode"
    - "gentle-ai"
```

## Retry Semantics

The per-issue retry budget counts **only worker-attributable failed attempts** (the harness exited non-zero). With the default `max_retries_per_issue: 3`:

- **Worker failure 1 and 2:** the issue returns to `Queued` with `next_attempt_at = now + retry_backoff_seconds`.
- **Worker failure 3:** the issue transitions to `Failed` and stops being claimed.
- **GitHub / git transport / local I/O / rate-limit / operator-cancellation failures:** do **not** consume the worker budget. They count as transient and the daemon retries on the next tick without bumping the per-issue counter.
- **Still-open `Failed` issues are not auto-reset.** Removing and re-adding the trigger label is not enough; use `caduceus queue reset OWNER/REPO#N [--dry-run]` (the recovery procedure is documented below).

## Investigation vs. Code Tickets

Both ticket types share the same bridge contract and the same `CADUCEUS_*` environment. They differ at finalization time:

| Phase result | Code ticket (`🤖 auto-fix`) | Investigation ticket (`🤖 auto-fix-investigate`) |
|---|---|---|
| Polling, claim, prompt, worker | same | same |
| Worker success result | commit + push + open PR + post completion comment + close issue | post findings comment + remove trigger label + leave issue open |
| Worker success schemas | `worker-result.json` fields all meaningful | `commit_message` / `pull_request_title` still required for schema stability but ignored |
| No commit, push, or PR is ever created for an investigation result | | |

The labels pass through `CADUCEUS_ISSUE_LABELS_JSON` as a JSON array of strings. The bridge forwards that array to the harness, and the harness decides how to branch. The bridge itself never forks behavior.

## Dry-Run Behavior

`CADUCEUS_DRY_RUN=1` (or `dry_run: true` in YAML) runs a full tick that performs **polling, claim, issue fetch, prompt creation, worker execution, result validation, and change inspection** — but does **not** commit, push, comment, mutate labels, create a PR, or close the issue. The daemon writes `<state_dir>/runs/<run_id>.dry-run.md` before teardown so you can audit what would have changed.

Successful dry-runs transition to `Previewed`. While dry-run remains enabled, rediscovery is a no-op (you won't get a queue of previews). The moment dry-run is disabled, any still-labeled `Previewed` entry is atomically promoted back to `Queued` so previewing never prevents the eventual real run.

## State Recovery Procedure

Caduceus stores queue state under `<state_dir>/queue.json` and metadata under `<state_dir>/state_meta.json`. Both files use temp-file + `fsync` + atomic rename and are never silently truncated:

- **Corrupt `queue.json`:** the daemon exits non-zero (exit 1) and preserves the corrupt file in place. Inspect `<state_dir>/queue.json`, repair it manually or use `caduceus migrate-state --from <path> [--dry-run]`, then re-run.
- **Corrupt `state_meta.json`:** same behavior — exit 1, file preserved, no silent empty-state overwrite.
- **Heartbeats older than 90 seconds are stale.** A live worker must refresh its heartbeat every 5 seconds; stale heartbeats are reaped on the next tick after `stale_run_hours` elapses.
- **Stuck issues:** requeued via `caduceus queue reset OWNER/REPO#N [--dry-run]`. The reset requires the daemon's whole-tick lock and refuses to drop an entry with an open PR unless `--force-finalization-reset` is supplied and confirmed in dry-run output.
- **Manual intervention is not the normal path.** Never edit state files directly; the daemon's lock + atomic-write discipline only holds for the programmatic API.

## Session Transcripts

Every worker invocation writes its transcript to `<state_dir>/runs/<run_id>.log`. The path is reported live in `caduceus status` so you can `tail -n 100` the most recent run without grepping the daemon log. Transcripts are byte-capped at `transcript_max_bytes` (default 10 MiB) with drain continuation — the daemon never silently drops output.

## Configuration Resolution

The daemon resolves its configuration in this order:

1. `$CADUCEUS_CONFIG` environment variable (path to a YAML file)
2. `$HERMES_HOME/config.yaml` under the `caduceus:` section (`HERMES_HOME` defaults to `~/.hermes`)
3. `~/.config/caduceus/config.yaml` (XDG-style, used as a fallback when running standalone without Hermes)

Hermes users get the standard path (option 2). Standalone users get the XDG path (option 3). Power users can override either with the env var.

## License

This project is open-source and available under the MIT License.
