# Auto Review

Auto Review is automated PR code review. The daemon polls watched
repositories for open pull requests, detects new head revisions, and runs
each one through an isolated, read-only OCI worker. The result is a
structured PASS/FAIL verdict published as a single sticky PR comment.

The canonical engineering specification is
[docs/architecture/auto-review.md](architecture/auto-review.md). This page
is the operator-facing reference; the architecture doc is the deep-dive.

## What Auto Review does

- Monitors PRs in `watched_repos` through the existing cron tick.
- Treats each previously unseen head revision as an immutable review
  unit — identity is `repository + PR number + head SHA`
  (DAR §2.1).
- Runs the review in a detached-HEAD worktree at the exact frozen
  revision, using merge-base (three-dot) diff semantics
  (DAR §2.2, §2.3).
- Produces a structured verdict: PASS or FAIL, with findings rated
  Blocking, Warning, or Suggestion, each carrying a title, body, optional
  path/line, and remediation guidance (DAR §3).
- Publishes one stable, idempotent sticky comment per PR, updated in
  place on new revisions (DAR §9).
- Re-reviews new revisions automatically; no manual trigger is needed.

What it does **not** do in Phase 1: no GitHub Checks API, no inline
comments, no auto-merge, no fork-PR review, no coalescing of
intermediate revisions. Same-SHA explicit re-review arrives in Phase 2
via a trusted PR comment (see below). See DAR §1 for the full
non-goal list.

## Enabling Auto Review

Auto Review requires OCI execution in Phase 1. The minimal config looks
like this:

```yaml
executor_mode: oci
sandbox:
  image: registry.example.com/caduceus-worker@sha256:<64-hex-digest>
  # ... other sandbox fields as documented in the wiki
auto_review:
  enabled: true
```

Before enabling, run `caduceus doctor` to confirm OCI readiness.

### TrustedHost is rejected

If `auto_review.enabled` is `true` and `executor_mode` is
`trusted_host`, config load fails with this deliberate error:

```text
auto_review.enabled requires OCI execution: set executor_mode: oci
and provide a valid sandbox: section (digest-pinned image).
TrustedHost offers no containment for reviewing untrusted PR
content. Run `caduceus doctor` or see the configuration docs
```

The fix is both changes named in the message: switch to
`executor_mode: oci` and provide a valid `sandbox:` block with a
digest-pinned image.

## How reviews are discovered and run

PR polling is a step inside the existing daemon tick, between issue
polling and the queue drain (DAR §5).

### Eligibility

A PR is admitted for review only when **all** of the following hold:

- The PR is open.
- The PR is not a draft, unless `draft_pull_requests: true` is set.
- The head SHA has not been reviewed before and is not already queued.
- The PR is not a fork, unless the base repo is listed in
  `auto_review.fork_policy.allow_fork_prs` (Phase 2, #337). Allowed
  forks are reviewed through a per-PR quarantine clone; see
  [docs/security/fork-trust-posture.md](security/fork-trust-posture.md).

When a PR does not qualify, the daemon emits a structured skip event
rather than silently ignoring it (DAR §5.1):

| Condition | Event |
|---|---|
| Draft PR | `review_skipped_draft` |
| Fork PR (not in `allow_fork_prs`) | `review_skipped_fork_unsupported` |
| Already-reviewed SHA | `review_skipped_already_complete` |
| Closed or merged PR | Never admitted |

### Admission budget

`max_reviews_per_tick` (top-level config, default
`worker_parallelism × 4`) caps how many reviews a single tick will
admit. `0` means unbounded. This prevents one busy repository from
flooding the shared worker pool.

### Revision-triggered re-review

When a PR pushes a new head SHA, the next poll discovers it and admits
a new review. The old review against the old SHA stays valid and
finalizable; the new review is a separate run. The head SHA is frozen
at discovery and never re-resolved (DAR §2.1).

### Explicit re-review via a trusted comment (Phase 2)

An allowlisted author can request a re-review of the current head SHA
by commenting the trigger command on the PR:

```text
/caduceus review
```

- The default command is `/caduceus review`; configure
  `auto_review.rerun_command` to change it. Matching is
  case-insensitive and whitespace-tolerant, but the command must be
  its own line — `/caduceus review` inside a longer sentence does
  **not** match, and a line inside a fenced code block (```` ``` ````
  or `~~~`) is ignored.
- Only authors on the top-level `feedback_author_allowlist` can
  trigger. An untrusted author's trigger is ignored and recorded as
  `review_rerun_skipped_untrusted`; with an empty allowlist no
  trigger is ever accepted (fail-closed).
- A trusted trigger enqueues an explicit re-review of the PR's
  **current** head SHA — even if that SHA was already reviewed. Each
  explicit run appends its own history row for the same SHA; no
  schema change is involved. Each trigger comment fires **exactly
  once**: once a review has been queued for it, the same comment is
  ignored on later polls (`skipped_no_trigger`), so a persistent
  comment cannot re-enqueue the same review every tick. Posting a
  **new** trigger comment starts a new review.
- While a review is already running (`InProgress`), a fresh trigger is
  skipped with `review_rerun_skipped_in_progress` instead of
  replacing the active run — the request fires once the current
  review finishes. Re-running never abandons a running review.
- Polling never does this: automatic discovery still skips
  already-reviewed SHAs with `review_skipped_already_complete`.

See DAR §17 for the full design.

### Draft behaviour

By default, draft PRs are skipped with `review_skipped_draft`. Set
`auto_review.draft_pull_requests: true` to admit them.

### Run and attempt semantics

The queue entry owns execution attempts; `ReviewState` has no
`attempt_count` field. Publication retries are separate from worker
retries and are tracked on `ReviewState.publication_attempt_count`
(DAR §3, §9.1).

## The review verdict and sticky comment

### Verdict vs execution status

These are intentionally distinct and must never be conflated in logs or
operator reading (DAR §8, §13):

- **Execution status** (`Success` or `Failure`) — did the review
  execute? Drives retry.
- **Verdict** (`Pass` or `Fail`) — did the code pass? Drives
  publication only.

A FAIL verdict is a successful execution; a FAILED status is an
infrastructure failure. The CLI separates the two fields for the same
reason (see [docs/cli.md](cli.md)).

### Findings

Each finding carries:

- `severity`: `Blocking`, `Warning`, or `Suggestion`
- `title`, `body`
- Optional `path` (repo-relative, no leading `/`), `line` (1-based,
  requires a path)
- Optional `remediation`

### Sticky comment

One comment per PR, marked with `<!-- caduceus-auto-review -->`. The
comment is updated in place on new revisions; superseded generations are
suppressed by a monotonic publication guard so an older run can never
overwrite a newer one (DAR §9.4). Re-publishing the same result is
byte-identical (idempotency requirement).

Re-reviews (generation 2 and later, `update` mode) prepend a
`> [!IMPORTANT]` banner naming the new reviewed commit and the
generation, so the update is visible without opening the comment's edit
history.

`auto_review.publication_mode` (`update` (default) | `new_comment`)
selects the re-review publication policy:

| Mode | Re-review behaviour | #393 banner | History |
|---|---|---|---|
| `update` (default) | PATCHes the single sticky comment in place | shown | one comment ever |
| `new_comment` | publishes a fresh comment per review generation | suppressed | every generation preserved |

`update` keeps the pre-#394 behaviour: one sticky comment per PR,
PATCHed on each re-review. `new_comment` publishes a fresh comment per
review generation and never edits history — the full comment trail per
re-review is kept. Markers are generation-tagged
(`<!-- caduceus-auto-review gen=N -->`); untagged pre-#394 comments
parse as generation 0. Crash-heal and gone-state marker adoption stay
exactly-once per generation in both modes; an unknown value fails the
config load.

## Config reference

All keys below are validated at load time; `deny_unknown_fields` is on
both the top-level `Config` and the `AutoReviewConfig` block, so a
typo'd key is a load failure, not a silent ignore.

| Key (YAML) | Type | Default | Source | Notes |
|---|---|---|---|---|
| `auto_review` | block | absent (disabled) | `src/infra/config/mod.rs:213` | Absent means disabled; no downstream code may read it |
| `auto_review.enabled` | `bool` | `false` | `:216` | Explicit Phase-1 opt-in |
| `auto_review.draft_pull_requests` | `bool` | `false` | `:219` | When `false`, drafts skip with `review_skipped_draft` |
| `auto_review.rerun_command` | `string` | `/caduceus review` | `:226` | Trusted-comment re-review trigger (DAR §17); must be non-empty and start with `/` |
| `max_reviews_per_tick` | `u32` | `worker_parallelism × 4` | `:303` | Top-level; `0` = unbounded |
| `state_backend` | `String` | `"json"` | `:244` | `"json"` or `"sqlite"`; review supports both |
| `executor_mode` | `ExecutorKind` | `TrustedHost` | `:841` | Auto Review requires `oci` |
| `sandbox` | block | absent | `:860-865` | Required when `executor_mode: oci` |

### The `autoreview` label

The `autoreview` GitHub label is **reserved and inert in Phase 1**:

- No daemon code polls it.
- No PR eligibility requires it.
- It must never be applied to issues as a classification label.

**Dispatch vs classification:** dispatch is flag-based
(`auto_review.enabled: true`). The `autoreview` label is a reserved
classification name with no dispatch effect in Phase 1. The canonical
spec for this distinction is DAR §5.2.

## Observability

Auto Review emits 23 structured event names during discovery,
dispatch, execution, finalization, and migration. They are listed in
DAR §13 and pinned by `review_event_catalog_test`; the authoritative
list lives in the architecture doc and should be read there rather than
duplicated here.

Use the review CLI to inspect live state:

```bash
caduceus review status [<owner/repo>] [--json]
caduceus review list [--json]
caduceus review show <owner/repo> <pr> [--json]
```

See [docs/cli.md](cli.md) for the full per-row field list and the
`review/1.0` JSON envelope.

## Troubleshooting

### "Auto Review will not start" — TrustedHost rejection

Symptom: `caduceus run` fails at config load with the error quoted in
[Enabling Auto Review](#enabling-auto-review).

Fix: switch to `executor_mode: oci` and add a valid `sandbox:` block
with a digest-pinned image. Run `caduceus doctor` to verify readiness.

### "Reviews are not appearing"

Checklist:

1. Is `auto_review.enabled: true` present in the config?
2. Does `caduceus doctor` report `READY` or `UNAVAILABLE`?
3. Does `caduceus review status` show queued or in-progress entries?
4. Are PRs open, non-draft (or `draft_pull_requests: true` set), and
   not forks?
5. Check the daemon log for skip events (`review_skipped_draft`,
   `review_skipped_fork_unsupported`, etc.).

### Stuck review

Run `caduceus review show <owner/repo> <pr>` to see the review state,
verdict, publication state, and last error. A review may be genuinely
in progress, or it may be blocked by:

- A mutation violation (`review_mutation_violation`) — terminal,
  routes to `NeedsAttention`. Retry cannot fix a contract violation.
- An unavailable head SHA (`review_skipped_head_sha_unavailable`) —
  the PR was force-pushed and the old SHA is gone. The next poll
  discovers the new SHA and admits a new review. Not
  `NeedsAttention`.
- A gone PR (`review_skipped_pr_gone` with reason
  `pr_not_found` or `closed_unmerged`) — permanently moot. Not
  `NeedsAttention`.

### Failed finalization or publication

If the sticky comment fails to publish, `ReviewState` enters
`FailedRetryable` with `publication_attempt_count`,
`next_publish_at`, and `last_publish_error` persisted. The model is
never re-run because publication failed; resume is idempotent via
`sticky_comment_id` (DAR §9.1).

### Draft PRs are skipped

Set `auto_review.draft_pull_requests: true` to admit them. The default
is `false`.

### Fork PRs are skipped

Fork PRs — and PRs whose head repository cannot be identified — are
unconditionally skipped in Phase 1 with
`review_skipped_fork_unsupported`. There is no config knob to enable
fork review (DAR §11.2).

### Oversized PR

If the diff alone exceeds the 1 MiB budget, the run is skipped via
`review_skipped_oversized_pr` without consuming the normal worker
retry budget. The event is deterministic: retrying cannot change
unreviewable input (DAR §7.1).
