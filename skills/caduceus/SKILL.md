# Caduceus Skill

Resolves as `caduceus:caduceus` after the plugin is loaded on Hermes Agent
v0.22.x (minimum tested host). Plugin skills are **opt-in**: they do not
auto-trigger. Load with `skill_view("caduceus:caduceus")` when the user asks
about the daemon.


## When to load

- Status / queue / “why isn’t issue #N claimed?”
- Configure watched repos, OCI sandbox, or Auto Review
- Swap harness (`harness.py` / bridge)
- Recover stuck tickets or reviews; corrupt state
- Doctor / logs / dry-run questions

## Product model

Caduceus is a Unix one-shot tick daemon. Cron (or Hermes) invokes
`caduceus run` about every two minutes. Bare `caduceus` prints help and
exits 0 — it does **not** run a tick.

| Job | Trigger | Executor |
| --- | --- | --- |
| Auto-review PRs | `auto_review.enabled: true` | **OCI required** |
| Implement tickets | label `autofix` (`ticket_label_code`) | TrustedHost or OCI |

- Sticky PASS/FAIL review comment; trusted `/caduceus review` re-runs
  (authors on `feedback_author_allowlist`; empty allowlist = fail-closed).
- `autoreview` GitHub label is reserved and **inert**.
- Investigation tickets / `ticket_label_investigation` are **removed**
  (unknown field fails config load).
- Fork PRs skipped unless base repo is in
  `auto_review.fork_policy.allow_fork_prs` (quarantine clone).
- Unknown config keys fail load (`deny_unknown_fields`).
- Pull-only; no auto-merge; worker never receives GitHub tokens.

## Workflow

1. **Status:** `/caduceus-status` or `caduceus status` / `status --json`.
2. **What’s happening:** `caduceus queue show`,
   `caduceus review status|list|show`; errors via
   `hermes caduceus logs` or `<state_dir>/processor.log`.
3. **Configure:** `caduceus:` in `~/.hermes/config.yaml` (or
   `$CADUCEUS_CONFIG` / `~/.config/caduceus/config.yaml`). Confirm each
   watched repo exists at `<workdir_base>/<owner>/<repo>` with a working
   noninteractive `origin`.
4. **Harness:** prefer copy
   `plugin-assets/caduceus_harness.py.example` →
   `$HERMES_HOME/caduceus/harness.py` implementing `run_task(ctx)`.
   Legacy `worker-bridge.py` still works. Re-seed with
   `hermes caduceus setup` before enabling PR review (PR env contract).
5. **Broken ticket:** `caduceus queue show OWNER/REPO#N`, then
   `queue reset` / `reprocess` / `remove` with `--dry-run` first.
   **Never edit state files.**
6. **Stuck review:** `caduceus review show OWNER/REPO PR` — check
   phase, verdict, publication state, last error.
7. **Dry-run:** `CADUCEUS_DRY_RUN=1` or `dry_run: true` — no
   commit/push/comment/PR/label/close; writes
   `<state_dir>/runs/<run_id>.dry-run.md`.

## Enable Auto Review

```yaml
caduceus:
  executor_mode: oci
  sandbox:
    image: "registry.example.com/caduceus-worker@sha256:<64-hex>"
  auto_review:
    enabled: true
```

TrustedHost + `auto_review.enabled: true` fails at config load. Run
`hermes caduceus doctor` / `caduceus doctor` first (want OCI `READY`).

Useful keys: `draft_pull_requests`, `rerun_command` (default
`/caduceus review`), `publication_mode` (`update`|`new_comment`),
`fork_policy.allow_fork_prs`, top-level `max_reviews_per_tick`.

## Config keys operators touch

Resolution: `$CADUCEUS_CONFIG` → `$HERMES_HOME/config.yaml` `caduceus:` →
`~/.config/caduceus/config.yaml` `caduceus:`.

| Key | Notes |
| --- | --- |
| `watched_repos` | `owner/repo` list; empty = discover |
| `ticket_label_code` | default `autofix`; legacy emoji remaps at read |
| `worker_command` | required for standalone |
| `worker_parallelism` | concurrent workers |
| `poll_interval_seconds` | default `120` |
| `workdir_base` | default `~/projects` |
| `executor_mode` | `trusted_host` or `oci` |
| `reduced_containment_acknowledged` | must be `true` for TrustedHost |
| `sandbox` | required for OCI (digest-pinned `image`) |
| `auto_review` | opt-in PR review block |
| `dry_run` | preview without GitHub side effects |
| `state_backend` | `json` (default) or `sqlite` |

`sandbox.network`: `none` (loopback) or `unrestricted` (engine bridge —
never host).

## CLI (binary)

```text
caduceus run
caduceus status [--json]
caduceus doctor [--json] [--skip-canary] ...
caduceus worktree-gc [--older-than-days N] [--dry-run]
caduceus queue show|reset|reprocess|remove ...
caduceus review status|list|show ...
caduceus migrate-state --from <path>|--to-sqlite [--dry-run]
caduceus setup [--dry-run]
```

Hermes wrappers: `hermes caduceus setup|doctor|status|logs|cron-install|
cron-remove` plus passthrough of `run`, `queue`, `review`, `worktree-gc`,
`migrate-state`. Slash: `/caduceus-status`.

## Worker contracts

Shared env: `CADUCEUS_CONTEXT_JSON`, `CADUCEUS_RUN_ID`,
`CADUCEUS_WORKTREE_PATH`, `CADUCEUS_RESULT_PATH`.

Issue path: `CADUCEUS_ISSUE_*`, `CADUCEUS_BRANCH_NAME`. Result:
`worker-result.json` with `status`, `summary`, `commit_message`,
`pull_request_title`.

PR path: `CADUCEUS_WORK_TARGET=pr`, `CADUCEUS_PR_*`. Result: review
schema at `CADUCEUS_RESULT_PATH` (required; no issue-path fallback).

Denied always: `GITHUB_TOKEN`, `CADUCEUS_GITHUB_TOKEN`, `GH_TOKEN`, and
related credential names.

## Doctor

`hermes caduceus doctor` combines install checks with
`caduceus doctor --json --skip-canary` and `caduceus status --json`.

- OCI `READY` OK; `UNAVAILABLE` is WARN on TrustedHost (expected).
- Tick age: under 5m OK, 5–30m WARN, 30m+ FAIL (cron ~2 min).
- Exit: 0 healthy, 1 config/runtime, 2 host capability. WARN does not
  change exit code.

## Recovery (CLI only)

Never hand-edit state, claims, or transcripts.

| Situation | Command |
| --- | --- |
| Inspect ticket | `queue show OWNER/REPO#N` |
| Retry Failed/Skipped | `queue reset OWNER/REPO#N [--dry-run]` |
| Fresh generation | `queue reprocess OWNER/REPO#N [--dry-run]` |
| Drop queue row | `queue remove OWNER/REPO#N [--dry-run]` |
| Stuck review | `review show OWNER/REPO PR` |
| Stale worktrees | `worktree-gc` |
| Backend switch | `migrate-state --to-sqlite` |

Re-adding the trigger label does **not** clear the worker retry budget.

## Lifecycle (Hermes)

```bash
hermes plugins install barkley-assistant/caduceus --enable
hermes caduceus setup
hermes caduceus cron-install
# later
hermes plugins update caduceus
hermes caduceus setup
hermes caduceus cron-remove
hermes plugins remove caduceus
```

Standalone: set `worker_command` explicitly. `caduceus setup` only writes
minimal non-secret config.

## Boundaries

- Do not edit daemon state, claims, or transcripts by hand.
- Do not change user config without confirmation.
- Bridge under `$HERMES_HOME/caduceus/` is operator-owned; setup never
  clobbers it (may write `.new`).
- Plugin does not auto-build; always run `hermes caduceus setup` after
  install/update.
