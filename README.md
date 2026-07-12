# Caduceus

Caduceus is a self-hosted, event-driven GitHub issue orchestrator and daemon written in Rust. It acts as a robust, secure supervisor that handles the tedious system infrastructure of autonomous issue management — ETag-aware polling, trust-tier classification, file-backed atomic queues, and isolated git worktrees — before handing off cognitive execution to any arbitrary, pluggable local worker script.

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
            (Sandboxed)      │ (No GitHub Credentials)
                             ▼
              ┌─────────────────────────────┐
              │      PLUGGABLE WORKER       │
              │  (Python / OpenCode / LLM)  │  <-- Your AI agent or script
              └──────────────┬──────────────┘
                             │
             Exit Status 0   │ Edits workspace files &
                             │ writes sdd-result.json
                             ▼
              ┌─────────────────────────────┐
              │       CADUCEUS DAEMON       │
              │        (Finalization)       │
              │  - Atomic Git Branch & Push │
              │  - Generates Pull Request   │
              │  - Closes Issue & Logs Run  │
              └─────────────────────────────┘
```

By explicitly decoupling deterministic infrastructure (Rust) from non-deterministic AI resolution (Python/TypeScript/Bash), Caduceus allows you to build rock-solid automation loops. Your AI agent scripts remain dead-simple, entirely air-gapped, and isolated from managing raw GitHub API states, secrets, or race conditions.

## Why Caduceus Exists

Bundling GitHub orchestration and AI-driven code generation in a single process is a debugging nightmare: a stalled LLM API call can hold the queue hostage, config files affect multiple subsystems at once, and a single hung subprocess is invisible until you `strace` it.

Caduceus fixes this by enforcing a strict boundary: the daemon owns **process lifecycle, IO, and atomicity guarantees**; the worker owns **code understanding and edits**. The worker is replaceable (Python script, OpenCode invocation, anything that respects a process contract), and the daemon is observable (`caduceus status` exposes runtime state at any moment).

## Key Features

- **Zero Inbound Networking:** Operates completely on an outbound pull model. No webhooks, no exposed ports, perfectly tailored for air-gapped or localized infrastructure.
- **Blazing Fast & Low Footprint:** Utilizes Rust's native `reqwest` client with full ETag-aware caching. If there are no new updates, it exits in milliseconds with zero memory overhead.
- **Hard Worker Timeout:** The daemon enforces `worker_timeout_seconds` on every worker invocation. If your LLM call hangs, the daemon kills the child, writes the transcript, and unlocks the queue. No more silent hangs.
- **Crash-Proof State Machine:** Centralized queue state is protected by robust OS-level file locks (`flock`) and per-issue atomic file creation primitives (`O_CREAT | O_EXCL`), rendering concurrency race-conditions impossible.
- **Isolated Workspace Sandboxing:** Every triggered issue is automatically provisioned inside its own temporary git worktree. If a worker script fails or goes rogue, your primary repositories remain completely untainted.
- **Secret Masking:** Your GitHub Personal Access Token (PAT) resides solely within Caduceus. Downstream AI worker scripts never see or touch your API credentials.
- **Bounded Retry Budget:** Each issue has a per-issue retry counter. After N failures, it transitions to a `failed` state and stops being claimed automatically — preventing infinite crash loops.

### The Single Worker Contract

Caduceus has exactly one worker path: **OpenCode with the `gentle-orchestrator` agent**. The worker reads `CADUCEUS_*` env vars, executes the SDD workflow, edits files in place, writes `sdd-result.json`, and exits with a code. There's no separate Python worker for investigations — investigation tickets (those with the `🤖 auto-fix-investigate` label) use the same OpenCode invocation, with the orchestrator's behavior driven by the label passed through `CADUCEUS_ISSUE_LABELS`.

The optional Python wrapper script (`examples/worker-opencode-sdd.sh`) is **not a competing worker**. It exists only to:

- Translate the `CADUCEUS_*` env vars into OpenCode's CLI flags
- Ensure the OpenCode invocation runs with the right working directory, model, and prompt file
- Surface the run ID and exit code in a way Caduceus's transcript capture can parse

If you prefer, you can skip the wrapper entirely and configure OpenCode directly via the `worker_command` array in your config.

### The Worker: OpenCode + Gentle-AI

The v0.1 worker is a single OpenCode invocation that delegates to the `gentle-orchestrator` agent. The worker reads the SDD prompt Caduceus writes to the worktree, lets gentle-ai drive the full Spec-Driven Development pipeline, edits files in place, writes `sdd-result.json`, and exits 0.

```yaml
worker_command:
  - "opencode"
  - "run"
  - "--agent"
  - "gentle-orchestrator"
  - "-f"
  - ".hermes-sdd-prompt.txt"
  - "--"
  - "Run the SDD workflow per the attached prompt file."
worker_timeout_seconds: 3600
```

For investigation tickets (`🤖 auto-fix-investigate` label), the same `worker_command` runs. The orchestrator's behavior differs based on the label, which Caduceus surfaces via the `CADUCEUS_ISSUE_LABELS` env var. The worker does **not** need to know it's an investigation ticket ahead of time — it reads the env var and behaves accordingly.

A thin optional wrapper (`examples/worker-opencode-sdd.sh`) is provided for users who want bash-level control over the OpenCode invocation (custom env, pre-flight checks, exit-code mapping). The wrapper is purely a translation layer between Caduceus's env contract and OpenCode's CLI surface; it does no domain logic of its own.

### Context Injection (Environment Variables)

Caduceus injects the absolute context of the issue directly into the child process environment:

| Variable | Purpose |
|---|---|
| `CADUCEUS_ISSUE_NUMBER` | The numeric ID of the active GitHub issue. |
| `CADUCEUS_ISSUE_TITLE` | The raw string title of the issue. |
| `CADUCEUS_ISSUE_BODY` | The core markdown body text of the issue. |
| `CADUCEUS_ISSUE_REPO` | Full `owner/repo` slug. |
| `CADUCEUS_ISSUE_LABELS` | Comma-separated list of current labels. |
| `CADUCEUS_WORKTREE_PATH` | Absolute path to the isolated directory where the worker is executing. |
| `CADUCEUS_RUN_ID` | Unique ULID/UUID identifying this run; used as the transcript log filename. |
| `CADUCEUS_CONTEXT_JSON` | Structured JSON compiling historical timeline, trusted edits, and allowed user comment threads for advanced multi-turn agent context tracking. |

Caduceus explicitly **does not** propagate `GITHUB_TOKEN`, `GH_TOKEN`, `AUTO_ISSUE_GITHUB_TOKEN`, or any credential to the worker. If your worker needs GitHub access, it must do so via a separate bot account token configured by you.

### Worker Resolution Expectation

Your script simply reads the environment variables, alters code files directly inside the active directory, and indicates its outcome via its system exit state:

- **Exit Code 0 (Success):** The script successfully resolved the issue. It must write a summary payload to `sdd-result.json` in the worktree root. Caduceus will then automatically create a branch, commit the changes, push to your remote, open a structured Pull Request, and close the origin ticket.
- **Non-Zero Exit Code (Failure/Abstain):** The script failed to find a valid solution or errored out. Caduceus catches the failure cleanly, captures the full stdout/stderr execution transcript to a dedicated runner log, tears down the worktree safely, and unlocks the queue.

### Required `sdd-result.json` Schema (For Exit 0):

```json
{
  "summary": "Detailed markdown text describing what the agent fixed. This becomes the description of the Pull Request.",
  "commit_message": "fix(core): resolve buffer allocation leak",
  "branch_name": "caduceus/auto-fix-42",
  "pull_request_title": "fix(core): automatic mitigation of memory allocation leak"
}
```

## Quick Start

### 1. Build and Install Caduceus

Ensure you have the Rust toolchain installed on your machine, then clone and compile:

```bash
git clone https://github.com/barkley-assistant/caduceus.git
cd caduceus
cargo build --release
cp target/release/caduceus ~/.hermes/bin/caduceus
```

### 2. Configure the Daemon

Create your configuration file at `~/.hermes/config.yaml`. This example bridges the Caduceus orchestrator with the OpenCode + Gentle-AI reference worker:

```yaml
caduceus:
  poll_interval_seconds: 120
  poll_user: "your-bot-account"
  state_dir: "~/.hermes/caduceus-state"
  log_path: "~/.hermes/caduceus-state/processor.log"
  sdd_workdir_base: "~/projects"

  # The pluggable execution worker definition:
  worker_command:
    - "opencode"
    - "run"
    - "--agent"
    - "gentle-orchestrator"
    - "-f"
    - ".hermes-sdd-prompt.txt"
    - "--"
    - "Run the SDD workflow per the attached prompt file."
  worker_timeout_seconds: 3600

  # Security Trust Tiers
  feedback_author_allowlist:
    - "trusted-maintainer-username"
    - "id:12345678"   # Numeric GitHub IDs supported to protect against rename spoofing
  comment_ignore_patterns: "dependabot\\[bot\\]|github-actions\\[bot\\]"

  # Bounded retry budget — after 3 failures, the issue is shelved
  max_retries_per_issue: 3

  # Activation Labels
  ticket_label_code: "🤖 auto-fix"
  ticket_label_investigation: "🤖 auto-fix-investigate"
```

> **Defaults are conservative — review before production.** The `poll_user` default `your-bot-account` and the `comment_ignore_patterns` regex are examples, not enforced values. Always review and override.

### 3. Create Labels In Your Target Repositories

```bash
gh label create "🤖 auto-fix" --repo OWNER/REPO --color "7C3AED" --description "Triggers Caduceus code automation pipeline"
gh label create "🤖 auto-fix-investigate" --repo OWNER/REPO --color "7C3AED" --description "Triggers Caduceus analysis summary"
```

### 4. Schedule Executions via Cron

Because Caduceus produces zero output if no work is detected or if an ETag yields an HTTP 304 Not Modified, it functions perfectly as a silent cron job:

```bash
# Example crontab entry running every 2 minutes
*/2 * * * * ~/.hermes/bin/caduceus >> ~/.hermes/caduceus-state/cron.log 2>&1
```

## Operational Diagnostics

### `caduceus status`

Inspect the daemon's runtime state without grepping logs:

```bash
$ caduceus status
Daemon version:     0.1.0
Last run started:   2026-07-12T21:50:00Z
Last run finished:  2026-07-12T21:50:03Z (304 Not Modified — idle tick)
Currently running:  no
Queued issues:      4
  - your-org/your-repo#338 (queued, attempts: 2)
  - your-org/your-repo#336 (error: label removed mid-run, attempts: 1)
  - your-org/your-repo#334 (error: label removed mid-run, attempts: 1)
  - your-org/your-repo#332 (queued, attempts: 0)
Stale claim reaped: 1 (last reap: 2026-07-12T21:30:00Z)
Rate limit reset:   2026-07-12T22:14:23Z (remaining: 4987/5000)
```

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

### Rate Limiting

If GitHub responds with `429` or `X-RateLimit-Remaining: 0`, Caduceus will:

- Log the rate-limit reset time
- Exit cleanly (cron will retry next tick)
- **Not** attempt any further API calls until `X-RateLimit-Reset`

### Worker Environment

Caduceus passes a sanitized environment to the worker. It explicitly **does not** pass `GITHUB_TOKEN`, `GH_TOKEN`, or any credential. The worker is responsible for ensuring its own runtime — for example, an `opencode`-based worker must have:

- The `opencode` binary on `PATH`
- A working provider/model configuration in `~/.config/opencode/opencode.json` (or via `OPENCODE_*` env vars)
- Any model-specific env vars (`OPENROUTER_API_KEY`, `OPENCODE_API_KEY`, etc.)

Caduceus logs the worker's resolved environment at startup (with secrets redacted) so you can debug `command not found` / provider-not-found issues.

## Configuration Variables Reference

| Key | Default | Purpose |
|---|---|---|
| `poll_interval_seconds` | `120` | Minimum execution cadence (will respect GitHub's `X-Poll-Interval` if longer). |
| `poll_user` | `your-bot-account` | The GitHub login profile whose event stream is analyzed. **Override this in production.** |
| `state_dir` | `~/.hermes/caduceus-state` | Location on disk where global states, atomicity lock-tokens, and running queues are managed. |
| `stale_run_hours` | `1` | Automatic crash-recovery threshold. Active issue claims older than this are reaped on next tick. |
| `worker_command` | *Required* | Array defining the exact command run to invoke your domain worker plug-in. |
| `worker_timeout_seconds` | `3600` | Hard timeout cap enforced by Caduceus before forcefully terminating a worker. |
| `sdd_workdir_base` | `~/projects` | Directory path where target repository clones are stored and worktrees are dynamically spun out. |
| `feedback_author_allowlist` | `()` | Logins or numeric user IDs whose comments/inputs are granted Trusted rank classification. |
| `comment_ignore_patterns` | standard bots | A regex list of users whose commentary additions should be explicitly skipped. |
| `max_retries_per_issue` | `3` | Maximum number of consecutive worker failures before an issue transitions to `failed` state and stops being claimed. |
| `ticket_label_code` | `🤖 auto-fix` | GitHub issue label used to queue structural pull-request code generation workflows. |
| `ticket_label_investigation` | `🤖 auto-fix-investigate` | GitHub issue label used to generate automated research descriptions without code PRs. |
| `comment_forbidden_strings` | see below | String list enforced on every bot comment (hard rule). See the "Public Voice Rule" section. |

## Public Voice Rule (Hard Enforcement)

The daemon refuses to post any bot comment containing the strings in `comment_forbidden_strings`. This is a **hard rule** — the daemon scans every outbound comment and skips the post if a forbidden string is found.

**Default forbidden strings:** `caduceus`, `opencode`, `gentle-ai`, `engram`, `sdd` (all matched case-insensitive substring).

**Why substring matching?** It catches every variant (`Caduceus`, `CADUCEUS`, `caduceus-bot`) without writing complex parsers. The cost is occasional false positives: the string `"sdd"` will match inside `"standards"` or `"address"`. If your comment needs to mention a string containing `sdd`, rephrase it ("Solid-State Drive" instead of "SSD") or override the list.

**Override the default list** by setting `comment_forbidden_strings` in your config. **Explicit values replace the defaults entirely** — they do not merge. This keeps the rule strict: a user who lists one forbidden string is signaling they've thought about the rule and want that exact list.

```yaml
caduceus:
  # Example: keep all defaults except remove the broad "sdd" matcher
  comment_forbidden_strings:
    - "caduceus"
    - "opencode"
    - "gentle-ai"
    - "engram"
```

## Hermes Integration (Optional)

Caduceus is a standalone tool and can be used without Hermes installed. If you are a Hermes user, the daemon will additionally look for config under the `caduceus:` section of `~/.hermes/config.yaml` as a fallback. Resolution order:

1. `$CADUCEUS_CONFIG` environment variable (path to a YAML file)
2. `~/.config/caduceus/config.yaml` (XDG-style, recommended for standalone users)
3. `~/.hermes/config.yaml` under the `caduceus:` section (Hermes users)

A small optional Hermes plugin (in the `plugin/` directory of this repo) provides:

- Auto-discovery of Caduceus state in the Hermes TUI
- `caduceus status` integration with the Hermes status surface
- Optional notifications via the existing Hermes Telegram gateway

The plugin is **fully optional** — Caduceus works without it. See `plugin/README.md` for installation.

## License

This project is open-source and available under the MIT License.