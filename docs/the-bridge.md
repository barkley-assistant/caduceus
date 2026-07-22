# The Bridge

This is the integration surface between Caduceus and
whatever AI harness you want to run. The daemon treats
the bridge as a black box that reads `CADUCEUS_*` env
vars and exits with a code. **The bridge is the only file
you should be editing if you want to swap harnesses.**

The README gives you the high-level shape; this doc is
the contract. Read it before you fork `worker-bridge.py`.

## What the Bridge Is

The bridge is a script the daemon spawns as a child of
the Rust worker supervisor. It is owned by the operator,
not by the daemon. Setup seeds a reference implementation
at `~/.hermes/caduceus/worker-bridge.py` (or your
standalone equivalent); you edit that file. **Plugin
source updates do not overwrite your edits** — that's the
whole point of the user-owned bridge pattern.

The reference implementation calls OpenCode with the
`gentle-orchestrator` agent. This is the harness the
project was prototyped against. The daemon has no
opinion about which harness the bridge invokes; you
swap the bridge for one that calls pi, codex,
claude-code, or your own custom script, and the daemon
will not notice or care.

## The Env-Var Contract

The daemon clears and rebuilds the worker's environment.
Every variable the bridge reads comes from this list.
**The daemon will never pass a GitHub credential to the
bridge.** This is not configurable.

- `CADUCEUS_ISSUE_NUMBER` (`string`) — The numeric issue ID.
- `CADUCEUS_ISSUE_TITLE` (`string`) — The issue title.
- `CADUCEUS_ISSUE_BODY` (`string`) — The issue body, raw Markdown.
- `CADUCEUS_ISSUE_REPO` (`string`) — `owner/repo` slug.
- `CADUCEUS_ISSUE_LABELS_JSON` (`JSON array`) — Current label names. The
  comma-separated form has been removed; use the array form.
- `CADUCEUS_WORKTREE_PATH` (`path`) — The isolated directory the worker is
  operating in.
- `CADUCEUS_RUN_ID` (`string`) — ULID/UUID naming this run; used as the
  transcript log filename.
- `CADUCEUS_CONTEXT_JSON` (`JSON object`) — Structured context
  (timeline, trusted edits, allowed comment threads) for advanced
  multi-turn harnesses. The schema, keys, and limits are
  documented in "The `CADUCEUS_CONTEXT_JSON` Schema" below.
- `CADUCEUS_BRANCH_NAME` (`string`) — The daemon-owned branch name. Workers
  may read it but must not create or rename branches.

Plus the inherited allowlist (configured by
`worker_env_allowlist`; defaults cover `PATH`, `HOME`,
`USER`, `SHELL`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, plus
`OPENAI_*`, `ANTHROPIC_*`, `OPENROUTER_*`, `OPENCODE_*`).

GitHub credential names (`GITHUB_TOKEN`, `GH_TOKEN`,
`CADUCEUS_GITHUB_TOKEN`, `AUTO_ISSUE_GITHUB_TOKEN`) are
**always denied**, even if you add them to
`worker_env_allowlist`. This is enforced in the daemon,
not in the bridge.

## The `worker-result.json` Contract

On exit 0 the bridge must leave a `worker-result.json`
at the worktree root. The schema is:

```json
{
  "status": "success",
  "summary": "Non-empty Markdown summary of what the worker did.",
  "commit_message": "fix(component): description",
  "pull_request_title": "fix(component): description",
  "artifacts": {
    "optional-name": "any JSON value"
  }
}
```

### Field Rules

- `status` is the literal string `"success"`. The daemon
  rejects any other value.
- `summary` is required, non-empty, NUL-free, limited to
  64 KiB. It becomes the PR description (code tickets)
  or the findings comment body (investigation tickets).
- `commit_message` is required for schema stability but
  is only used for code tickets. Newlines are allowed in
  commit messages; no other control characters. Limited
  to 256 characters.
- `pull_request_title` is required for schema stability
  but is only used for code tickets. One line, no control
  characters. Limited to 256 characters.
- `artifacts` is optional. When present it's a
  `BTreeMap<String, serde_json::Value>`. Keys are
  non-empty, control-free, ≤ 128 characters, ≤ 100
  entries. The daemon renders the artifact section in
  the PR body (code) or findings comment
  (investigation), escaped and size-limited.

### Size and Validation

- The whole file is limited to 1 MiB. The daemon reads
  it once and rejects anything larger.
- Unknown top-level fields are rejected. The daemon
  does not silently ignore them.
- `commit_message` and `pull_request_title` are required
  for schema stability on investigation tickets too,
  even though they're ignored. This is so a single
  harness can serve both ticket types without forking
  its result shape.

## The Exit-Code Contract

- `0` — Success. Reads `worker-result.json`, finalizes.
- Non-zero — Failure or abstain. Captures transcript, releases claim, retries
  per the retry budget.

The bridge may exit any non-zero value to signal
failure; the daemon does not inspect it beyond "is it
zero?". Pick a meaningful exit code in your bridge for
your own debugging, but the daemon treats every
non-zero the same.

## What the Bridge Must Not Do

The daemon does not enforce these as runtime checks; it
trusts the bridge because the bridge is your code. The
following are *strongly* discouraged because they
violate the contract Caduceus's design assumes:

- **Do not write to `~/.hermes/caduceus-state/` or
  `<state_dir>/`.** That directory is the daemon's.
  The bridge writes only inside its worktree.
- **Do not create or rename Git branches.** The daemon
  owns the branch name (`CADUCEUS_BRANCH_NAME`). If
  the bridge creates a different branch, the daemon's
  finalize step will not see the work and the issue
  fails. If the bridge renames the daemon's branch,
  the next tick will not be able to resume.
- **Do not interact with the GitHub API directly.** The
  bridge has no GitHub token (deliberately). If you
  need to read more data than `CADUCEUS_CONTEXT_JSON`
  provides, the bridge can read it from the local
  clone (which was fetched by the daemon), or you can
  ask the daemon to expose more via the context.
- **Do not install signal handlers.** The daemon's
  supervisor delivers SIGINT/SIGTERM to the worker
  session; installing your own handlers confuses the
  kill propagation and the worker may not die on
  timeout.
- **Do not fork daemon-like processes.** The supervisor
  is not a process reaper; a daemonised child of the
  bridge is its problem child, not the supervisor's.
  This is why "do not background yourself" is the most
  important rule of bridge-writing.
- **Do not retry the harness internally.** Retries are
  the daemon's job; the bridge is one invocation per
  exit.

## The Reference Bridge

The shipped `plugin-assets/worker-bridge.py` is a
reference implementation that calls OpenCode via its
`gentle-orchestrator` agent. It is the bridge that ships
with the Hermes plugin; it is also the bridge your
plugin setup seeds at `~/.hermes/caduceus/worker-bridge.py`.

The reference bridge:

- Validates the env vars up front and exits 2 on
  missing required vars (a distinct exit code from
  "harness failed" so the daemon's logs tell you which).
- Parses `CADUCEUS_ISSUE_LABELS_JSON` and forwards it
  to the harness.
- Invokes the harness via `subprocess.run` with an
  argument array (never `shell=True`), inheriting the
  bridge's stdin/stdout/stderr.
- Captures the harness's exit code and propagates it.
- Does **not** write `worker-result.json` itself — the
  harness does, because the harness is what knows what
  it did.

To plug in a different harness:

1. Edit your user-owned `worker-bridge.py`.
2. The `invoke_harness(worktree, prompt_file, run_id,
   labels, branch_name)` function is the single
   user-editable hook. Replace its body.
3. The rest of the bridge is harness-agnostic plumbing
   you don't need to touch.

## The Harness Contract (Informational)

This is the contract the bridge assumes about the
harness it spawns. Caduceus does not enforce it; the
bridge does.

- The harness reads its prompt from a path the bridge
  passes (or from stdin).
- The harness writes `worker-result.json` to the CWD
  (which the daemon sets to the worktree root).
- The harness exits 0 on success; any non-zero on
  failure.
- The harness does not need to handle `SIGINT` /
  `SIGTERM` cleanly; the daemon's supervisor will reap
  the whole session if the harness ignores them.

## The `CADUCEUS_CONTEXT_JSON` Schema

`CADUCEUS_CONTEXT_JSON` is normative. It is the only
documented channel for structured daemon-to-bridge
context and is the single extension point for adding
new read-only context without changing the
`CADUCEUS_*` env-var list. The schema is versioned;
the contract owns the version and key set. This
section is the authoritative reference for the
schema.

### Encoding and limits

- Single-line UTF-8 JSON object. The daemon emits a
  single object without trailing newlines and without
  indentation; the bridge parses with the standard
  `json.loads` path.
- Total size at most **64 KiB**. Anything larger is a
  daemon-side validation error; the bridge never sees
  it.
- No NUL bytes, no control characters other than
  `\n`, `\r`, and `\t`. Strings are trimmed only by the
  bridge; the daemon emits untrimmed JSON.

### Required keys

- `schema_version` (`integer`) — The context schema
  version. The current version is `1`. Bridges MUST
  refuse unknown `schema_version` values with a
  distinct non-zero exit so the daemon's logs say
  "context schema unsupported" rather than "harness
  failed".
- `issue` (`object`) — Snapshot of the issue the
  worker is operating on:
  - `number` (`integer`)
  - `title` (`string`, raw Markdown, ≤ 1 MiB)
  - `body` (`string`, raw Markdown, ≤ 1 MiB)
  - `repo` (`string`) — `owner/repo` slug
  - `labels` (`array<string>`) — current label names,
    authoritative; `CADUCEUS_ISSUE_LABELS_JSON` is the
    same data
  - `author` (`object`):
    - `login` (`string`)
    - `id` (`integer`)
  - `created_at` (`string`) — ISO-8601 timestamp
  - `updated_at` (`string`) — ISO-8601 timestamp

### Optional keys

- `timeline` (`array<object>`) — Ordered list of
  prior events on this issue generation. Each event:
  - `kind` (`string`) — one of `labeled`, `unlabeled`,
    `commented`, `assigned`, `reopened`, `closed`,
    `retargeted`, `reprocessed`
  - `at` (`string`) — ISO-8601 timestamp
  - `actor` (`object`) — same shape as `issue.author`
  - `summary` (`string`) — short, redacted free text
- `trusted_edits` (`array<object>`) — Edit history
  restricted to comments whose author passes
  `feedback_author_allowlist`. Each item mirrors a
  timeline event with `kind = "commented"` plus the
  full comment body in `body`.
- `allowed_comment_threads` (`array<object>`) —
  Thread roots the harness is permitted to interact
  with. Each item:
  - `id` (`integer`) — comment ID
  - `author` (`object`) — same shape as `issue.author`
  - `body_preview` (`string`, ≤ 256 chars)
  - `url` (`string`)
- `finalization_checkpoint` (`object | null`) — When
  the daemon is resuming from a durable checkpoint,
  this carries the prior state:
  - `stage` (`string`) — one of `Committed`, `Pushed`,
    `PrCreated`, `Commented`, `AwaitingReview`
  - `branch` (`string`)
  - `commit_oid` (`string`)
  - `pull_request_url` (`string | null`)
  - `run_id` (`string`)
- `daemon_diagnostics` (`object`) — Read-only hints
  about daemon state. The bridge MUST treat this as
  advisory only:
  - `attempt_number` (`integer`)
  - `attempt_budget_remaining` (`integer`)
  - `next_attempt_at` (`string | null`) — ISO-8601 or
    `null` if no retry is queued

### Redaction rules

The daemon emits the schema with these fields redacted:

- No GitHub credential names. `GITHUB_TOKEN`,
  `GH_TOKEN`, `CADUCEUS_GITHUB_TOKEN`,
  `AUTO_ISSUE_GITHUB_TOKEN` are never present in any
  value.
- No token-shaped strings. The daemon does not insert
  raw `gh*` or `*_TOKEN` values; if a comment or
  timeline summary contains one, it is replaced with
  the literal string `[REDACTED]`.
- No worker env values. `worker_env_allowlist` is
  applied before the daemon serializes the context.

### Stability rules

- New optional keys MAY be added in a minor version of
  the schema without bumping `schema_version`, as long
  as existing keys keep their types and meanings.
- Removing or retyping a key, or changing the meaning
  of an existing key, requires `schema_version` to
  increment and a `CONTRACT_REVISIONS.md` entry.
- The 64 KiB limit and the encoding rules are
  invariants; changing either is a contract revision.

## Troubleshooting the Bridge

- Bridge exits 2 with "missing required env var"
  - Likely cause: Daemon didn't pass one of the `CADUCEUS_*` vars
  - Fix: Check that `Config::load` succeeded; check the daemon log.
- Bridge exits 137 (killed by signal 9)
  - Likely cause: Timeout
  - Fix: `worker_timeout_seconds` is too short, or the harness is genuinely
    hung.
- Daemon reads `worker-result.json` but it's empty
  - Likely cause: Harness wrote 0 bytes
  - Fix: Harness contract violation; check the harness's own logs.
- Daemon reports "unknown field" on the result file
  - Likely cause: You added a top-level key not in the schema
  - Fix: Remove the key from your `worker-result.json`.
- Daemon reports `forbidden string` in the summary
  - Likely cause: Your summary text mentions one of the
    `comment_forbidden_strings` entries
  - Fix: See `public-voice.md`; either edit the summary or override the defaults
    list.

When in doubt, `caduceus status --json` and the daemon's
own log file (`<state_dir>/processor.log`) are the
authoritative sources for what happened.
