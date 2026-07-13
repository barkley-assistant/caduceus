# Caduceus

> **Hermes Agent plugin for self-hosted, event-driven GitHub issue automation.**

Caduceus is a Rust daemon that polls GitHub for labeled issues, runs a user-configured AI harness against them in isolated worktrees, enforces hard timeouts, and finalizes the result as a branch + push + PR. It's shipped as a **Hermes plugin** so it integrates with your existing agent profile, cron harness, and Telegram delivery — and you install the Rust binary alongside the plugin in one step.

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

By explicitly decoupling deterministic infrastructure (Rust) from non-deterministic AI resolution (Python/TypeScript/Bash), Caduceus allows you to build rock-solid automation loops. Your AI agent scripts remain dead-simple, entirely air-gapped, and isolated from managing raw GitHub API states, secrets, or race conditions.

Caduceus is **harness-agnostic**. Whether your worker invokes OpenCode, pi, codex, claude-code, or a custom agent, the contract is the same: the bridge reads `CADUCEUS_*` env vars, runs the harness, and the harness writes a `worker-result.json` file describing what it did. Caduceus never assumes any specific harness, workflow, or methodology.

## Installation (Hermes plugin)

```bash
# Add the plugin to your active Hermes profile
hermes plugin install barkley-assistant/caduceus

# This installs:
#   - The Rust daemon binary (managed by the plugin)
#   - The plugin's skills, commands, and cron profile
#   - The reference worker-bridge.py for OpenCode

# Configure (in your ~/.hermes/config.yaml under caduceus: section)
# See "Configuration" below

# Start the daemon — the plugin sets up a cron profile for you
hermes cron enable caduceus
```

The plugin manages the daemon binary lifecycle (install, upgrade, uninstall) so you never have to `curl | sh` or copy binaries manually.

## Installation (standalone — no Hermes)

If you don't use Hermes, you can install the daemon binary directly:

```bash
git clone https://github.com/barkley-assistant/caduceus.git
cd caduceus
cargo build --release
cp target/release/caduceus ~/.local/bin/caduceus

# Then create your config at ~/.config/caduceus/config.yaml
# See "Configuration" below
```

You lose the plugin's skill/command/Telegram integration, but the daemon works identically. Config in `~/.config/caduceus/config.yaml` is the recommended location when not using Hermes.

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

## The Single Worker Contract

Caduceus has exactly one worker path: a **harness bridge script** that you (the user) configure to invoke whichever AI harness you want — OpenCode today, pi or Codex or anything else tomorrow. The bridge receives `CADUCEUS_*` env vars from Caduceus, translates them into the harness's CLI surface, runs the harness, and exits with its exit code. Caduceus doesn't know or care which harness is on the other end.

For investigation tickets (`🤖 auto-fix-investigate` label), the same bridge script runs — its behavior is driven by the labels passed through `CADUCEUS_ISSUE_LABELS`.

The bridge is the **stable API** for harness integration. We ship a reference Python bridge (`worker-bridge.py` — installed by the plugin to `~/.hermes/profiles/<profile>/plugins/caduceus/`) that invokes OpenCode with the `gentle-orchestrator` agent, but **the bridge is a starting point, not a constraint**. To switch harnesses:

1. Edit your local copy of `worker-bridge.py` (the plugin keeps your version across upgrades)
2. Edit the `invoke_harness()` function to call your preferred harness's CLI (pi, codex, claude-code, etc.)
3. Caduceus keeps working unchanged

The bridge owns the translation between Caduceus's env-var contract and your harness's CLI surface. Everything else — worktree provisioning, timeouts, queue management, finalize — stays in Caduceus.

### The Worker: Harness Bridge Pattern

Caduceus spawns a **bridge script** (typically Python) that you configure to call whichever harness you want. We ship a reference bridge (`worker-bridge.py`) that invokes OpenCode with the `gentle-orchestrator` agent. You fork it to plug in a different harness — pi, codex, claude-code, or anything else.

**Out of the box (OpenCode):**

```python
# worker-bridge.py — reference implementation
def invoke_harness(worktree, prompt_file, run_id, labels):
    return subprocess.run([
        "opencode", "run",
        "--agent", "gentle-orchestrator",
        "-f", str(prompt_file),
        "--", "Run the workflow per the attached prompt file."
    ], cwd=worktree)
```

**Plugging in a different harness** (e.g., pi):

```python
# your-copy/worker-bridge.py — your edits
def invoke_harness(worktree, prompt_file, run_id, labels):
    return subprocess.run([
        "pi", "--workdir", str(worktree),
        "--prompt-file", str(prompt_file),
        "--run-id", run_id,
    ], cwd=worktree)
```

The bridge does only two things:

1. **Translate** `CADUCEUS_*` env vars into the harness's CLI flags
2. **Propagate** the harness's exit code so Caduceus's `worker_timeout_seconds` and transcript capture work correctly

Everything else — worktree provisioning, polling, atomic claims, finalize, comment posting — stays in Caduceus. The harness can be replaced without touching the daemon.

For investigation tickets (`🤖 auto-fix-investigate` label), the same bridge runs. The harness reads `CADUCEUS_ISSUE_LABELS` and behaves accordingly. The bridge does **not** need to fork its behavior — it just passes the labels through.

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

- **Exit Code 0 (Success):** The script successfully resolved the issue. It must write a result payload to `worker-result.json` in the worktree root. Caduceus will then automatically create a branch, commit the changes, push to your remote, open a structured Pull Request, and close the origin ticket.
- **Non-Zero Exit Code (Failure/Abstain):** The script failed to find a valid solution or errored out. Caduceus catches the failure cleanly, captures the full stdout/stderr execution transcript to a dedicated runner log, tears down the worktree safely, and unlocks the queue.

### Required `worker-result.json` Schema (For Exit 0):

```json
{
  "status": "success",
  "summary": "Detailed markdown text describing what was done. Becomes the description of the Pull Request.",
  "branch_name": "your-name/auto-fix-42",
  "commit_message": "fix(component): resolve the issue",
  "pull_request_title": "fix(component): automatic mitigation of issue"
}
```

All fields are required. The schema is deliberately minimal and harness-agnostic — Caduceus doesn't care whether the worker used SDD, TDD, freeform editing, or anything else. It only needs to know the branch to push, the commit message to use, and the PR title/body to create.

If your harness produces additional artifacts (specs, designs, test outputs, logs), you can include them under an optional `artifacts` object. Caduceus will surface them in the PR description but does not interpret their contents:

```json
{
  "status": "success",
  "summary": "...",
  "branch_name": "...",
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

When installed as a Hermes plugin, Caduceus adds the following to your agent profile:

- **`caduceus` skill** — Triggered when you mention GitHub issues, auto-fix, or PR automation in chat. Walks you through configuration, shows queue state, and surfaces recent run history.
- **`/caduceus-status` command** — Quick status check from your chat surface (TUI or Telegram). Shows queue contents, last-run timestamps, current retry budget.
- **`caduceus` cron profile** — Runs `caduceus` every 2 minutes. Configurable cadence.
- **Daemon lifecycle management** — `hermes plugin upgrade caduceus` rebuilds and re-installs the binary. `hermes plugin uninstall caduceus` cleanly tears down state and cron profile.

The plugin's SKILL.md, commands, and manifests live in `plugin/` in this repo.

## Quick Start (Plugin path)

Once installed via `hermes plugin install barkley-assistant/caduceus`:

### 1. Configure

Add to your `~/.hermes/config.yaml`:

```yaml
caduceus:
  poll_interval_seconds: 120
  poll_user: "your-bot-account"
  state_dir: "~/.hermes/caduceus-state"
  log_path: "~/.hermes/caduceus-state/processor.log"
  workdir_base: "~/projects"

  # The pluggable execution worker definition.
  # By default this points at the bundled bridge script, which invokes
  # OpenCode with the gentle-orchestrator agent. Edit the bridge (or
  # write your own) to plug in pi, codex, claude-code, or any other
  # harness — Caduceus doesn't care which one is on the other end.
  worker_command:
    - "python3"
    - "/path/to/your/worker-bridge.py"
  worker_timeout_seconds: 3600

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

  # Activation Labels
  ticket_label_code: "🤖 auto-fix"
  ticket_label_investigation: "🤖 auto-fix-investigate"
```

> **Defaults are conservative — review before production.** The `poll_user` default `your-bot-account` is an example, not an enforced value. Always review and override.

**About the allowlist:** Each entry is either a GitHub login or `id:<numeric>` where `<numeric>` is the user's GitHub numeric user ID (visible via `https://api.github.com/users/<login>` in the `id` field). Numeric IDs are recommended for security-sensitive contexts because they survive username renames — a user who renames their account to bypass an allowlist still matches the numeric ID. The daemon extracts the numeric ID from each comment's `user.id` field at fetch time; no extra API call is required.

**About `comment_ignore_patterns`:** A list of regex patterns matched against each comment author's login (substring match, case-sensitive). The default patterns filter standard bot accounts (`dependabot[bot]`, `github-actions[bot]`) so their automated comments don't pollute the worker context. The list **replaces** the defaults if you set any values — to keep the defaults, set the list back to both patterns explicitly.

### 2. Create Labels In Your Target Repositories

```bash
gh label create "🤖 auto-fix" --repo OWNER/REPO --color "7C3AED" --description "Triggers Caduceus code automation pipeline"
gh label create "🤖 auto-fix-investigate" --repo OWNER/REPO --color "7C3AED" --description "Triggers Caduceus analysis summary"
```

### 3. Enable the cron profile

```bash
hermes cron enable caduceus
```

Caduceus is now running silently every 2 minutes.

## Operational Diagnostics

### `caduceus status` (CLI)

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
| `worker_command` | *Required* | Array defining the exact command run to invoke your harness bridge. Default points at the plugin's bundled `worker-bridge.py`. |
| `worker_timeout_seconds` | `3600` | Hard timeout cap enforced by Caduceus before forcefully terminating a worker. |
| `workdir_base` | `~/projects` | Directory path where target repository clones are stored and worktrees are dynamically spun out. |
| `feedback_author_allowlist` | `()` | Logins or numeric user IDs whose comments/inputs are granted Trusted rank classification. |
| `comment_ignore_patterns` | standard bots | A regex list of users whose commentary additions should be explicitly skipped. |
| `max_retries_per_issue` | `3` | Maximum number of consecutive worker failures before an issue transitions to `failed` state and stops being claimed. |
| `ticket_label_code` | `🤖 auto-fix` | GitHub issue label used to queue structural pull-request code generation workflows. |
| `ticket_label_investigation` | `🤖 auto-fix-investigate` | GitHub issue label used to generate automated research descriptions without code PRs. |
| `comment_forbidden_strings` | see below | String list enforced on every bot comment (hard rule). See the "Public Voice Rule" section. |

## Public Voice Rule (Hard Enforcement)

The daemon refuses to post any bot comment containing the strings in `comment_forbidden_strings`. This is a **hard rule** — the daemon scans every outbound comment and skips the post if a forbidden string is found.

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

## Configuration Resolution

The daemon resolves its configuration in this order:

1. `$CADUCEUS_CONFIG` environment variable (path to a YAML file)
2. `~/.hermes/config.yaml` under the `caduceus:` section (the standard location when installed as a Hermes plugin)
3. `~/.config/caduceus/config.yaml` (XDG-style, used as a fallback when running standalone without Hermes)

Hermes users get the standard path (option 2). Standalone users get the XDG path (option 3). Power users can override either with the env var.

## License

This project is open-source and available under the MIT License.