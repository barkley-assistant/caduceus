# Auto Review — Architecture and Engineering Specification

**Status:** Approved for implementation (Phase 1) · **Supersedes:** all in-conversation planning artefacts · **Scope:** release N (Phase 1) with release N+1 migration train

This document is the canonical engineering specification for Caduceus Auto Review. Every issue in the Auto Review epic (#290) references sections of this document; implementers never need the planning conversation. Where an ADR or spec-section reference appears in an issue, it resolves here.

---

## 1. Goals and non-goals

### Goals

- Monitor pull requests in configured repositories through the existing cron tick.
- Detect previously unseen PR head revisions and treat each as an immutable unit of review.
- Run each review in an isolated, read-only worker against the exact observed revision.
- Produce a structured PASS/FAIL verdict with actionable findings and remediation guidance.
- Publish results as one stable, idempotent sticky PR comment per PR.
- Preserve Caduceus's crash-safety, retry, idempotency, and security guarantees.
- Automatically re-review new revisions without user interaction.

### Non-goals (Phase 1)

- GitHub Actions; GitHub App; Checks API; branch-protection checks; inline review comments.
- Automatic code mutation after a failed review; automatic merging.
- Multi-agent review orchestration; conversational PR interaction.
- Replacing CI tests; reviewing historical PRs on enablement.
- Fork-PR review (hard-gated out; Phase 2 trust policy).
- Same-SHA explicit re-review (Phase 2, `/caduceus review`).
- Coalescing of intermediate revisions (every admitted SHA is reviewed).

---

## 2. Review identity and diff semantics

### 2.1 Identity

**Immutable review revision identity = `repository + PR number + head SHA`.** The head SHA is captured at discovery, frozen into the queue entry, and **never re-resolved** afterward. If the PR moves on while a review runs, that review completes against the originally observed SHA and remains valid and finalizable; the next poll discovers the new SHA and admits it as a new target.

**Review context** (carried alongside identity, not part of it): base SHA, base ref, and the computed **merge base**. Base movement with an unchanged head SHA does **not** create a new review identity; if re-review on base movement is ever wanted, it is an explicit future policy, not an identity change.

### 2.2 Diff semantics

The review scope is the PR's actual changes, computed with **merge-base (three-dot) semantics**:

```
merge_base = git merge-base base_sha head_sha
git diff merge_base head_sha
```

Endpoint-to-endpoint (`base_sha..head_sha`) diffing is **prohibited** — it can include changes introduced independently on the base branch. `..` and `...` must never be used ambiguously anywhere in Caduceus; all review-diff code, prompts, and tests use the explicit merge-base form above. (`git merge-base` already runs through the hardened GitRunner for ancestor checks; the review diff computation reuses it.)

---

## 3. Domain model

```rust
// Immutable identity (frozen at discovery)
struct ReviewTarget {
    repository: RepositoryId,
    pull_request: u64,
    head_sha: String,          // identity component
    base_sha: String,          // context: for merge-base diff computation
    base_ref: String,          // context
}

// Per-(repo, pr) durable current-state pointer
struct ReviewState {
    repository: RepositoryId,
    pull_request: u64,
    last_reviewed_head_sha: Option<String>,
    last_verdict: Option<ReviewVerdict>,
    last_reviewed_at: Option<DateTime<Utc>>,
    sticky_comment_id: Option<u64>,
    last_run_id: Option<RunId>,
    review_generation: u64,        // monotonic per (repo, pr); see §8
    publication_state: PublicationState,   // Pending | Publishing | Published | FailedRetryable
    publication_attempt_count: u32,        // publication retries — NEVER the worker attempt counter
    next_publish_at: Option<DateTime<Utc>>,
    last_publish_error: Option<String>,
}

// Per-run structured result (canonical, presentation-independent)
struct ReviewResult {
    schema_version: u32,       // = 1 in Phase 1
    status: ExecutionStatus,   // Success | Failure — drives retry
    review: Option<Review>,    // present iff status == Success
}
struct Review {
    verdict: Verdict,          // Pass | Fail
    summary: String,
    findings: Vec<Finding>,    // order = persisted order (determinism requirement)
}
struct Finding {
    severity: Severity,        // Blocking | Warning | Suggestion
    title: String, body: String,
    path: Option<String>, line: Option<u32>,
    remediation: Option<String>,
}
```

**No `attempt_count` on `ReviewState`:** the queue entry owns execution attempts; `publication_attempt_count` owns publication retries; a third counter has no consumer.

**Field validation caps:** findings and string fields carry sane maximum sizes (enforced at parse/validation) so adversarial results cannot break rendering budgets (§10.3).

---

## 4. Queue and persistence model

### 4.1 Sibling review queue

Review work lives in a **sibling queue**, not a variant of the issue queue. The invariant: **review identity never enters `IssueKey`**. A serialized `"review"` ticket-type would hard-fail today's whole-store load — the sibling queue avoids forcing PR identity into an issue-only schema. `TicketType::Investigation` survives as a terminal-never-admitted variant for migration safety (§12).

### 4.2 Both state backends are first-class

`state_backend` is a validated config field (`"json"` | `"sqlite"`, default `"json"`) selected at one branch point. Auto Review supports **both**:

- **SQLite**: review queue, `ReviewState`, and `review_history` tables under the store envelope; envelope bumped **v7 → v8** when review state enters it (mandatory — writing review rows under v7 makes v7 mean two things per binary version). Unknown store version hard-fails at startup with operator instructions.
- **JSON**: the versioned `state.json` envelope (`QUEUE_FILE_VERSION` / `QueueState.version`, deny-unknown-fields) is bumped symmetrically. A pre-review daemon opening a JSON state file containing review entries hard-fails the whole load exactly like SQLite — the JSON bump prevents the same class of failure. JSON review history is a versioned sidecar under the same temp+fsync+rename discipline.

Both paths: crash-safe writes, validated loads, corrupted-input rejection, and migration coverage in tests.

### 4.3 Review history (per-run, not per-identity)

History identity is a **`review_run_id`/generation, unique per completed run** — NOT `(repo, pr, head_sha)` uniqueness. Indexes over `(repository)`, `(pull_request)`, and `(head_sha)` support lookup; the tuple is **not unique** because same-SHA re-review (Phase 2, `/caduceus review`) must append additional rows without a migration. Automatic-discovery dedup (skip already-reviewed SHAs) reads `ReviewState.last_reviewed_head_sha` and the active queue — dedup and historical-run identity are different concepts and are never conflated.

Rows store the **opaque, version-tagged raw `ReviewResult` JSON**. Old versions are read-only, never back-migrated; rendering is version-aware. The canonical result is the persisted blob (spec §31 of the original proposal): the PR comment is presentation only.

### 4.4 Migration framework

Migrations are **structural only** — never drain-by-execution (a migration whose behaviour depends on which binary opens the store is a version-semantics violation; the v6 precedent). The chain runs from any older version so a direct pre-N → N+1 upgrade fires identically. Release N+1 adds a **startup reconcile pass independent of the migration chain**, fired on every store open: any non-terminal investigation row is terminated + archived + audited (`review_migration_terminated_investigation`), because operators running N admit post-migration investigation rows that no migration will ever see.

---

## 5. Discovery and polling

PR polling is a step **inside the existing daemon tick**, between issue polling and the queue drain. Ordering invariants:

- **Per-repo log-and-continue**: one repo's PR-list failure must not abort issue polling for later repos (the issue loop's break-on-first-error shape is not inherited).
- PR-step failures route through the existing non-fatal outcome classifier.
- Eligibility predicate: open + non-draft + head SHA not already reviewed + not already queued + **not a fork** (§11.2).
- The head SHA is frozen at discovery; diff/context fetched only for genuinely new SHAs (rate-limit discipline: no full diffs when nothing changed).
- Per-tick admission budget `max_reviews_per_tick` (sibling default shape to `max_issues_per_tick`) prevents one busy repo from flooding the shared worker pool.

### 5.1 Discovery-time PR states

| State | Behaviour |
|---|---|
| open, non-draft, new SHA | admit |
| draft | skip, `review_skipped_draft` |
| fork (head.repo ≠ base.repo) | skip, `review_skipped_fork_unsupported` (carries repo, PR, head-repo identity or null) |
| closed / merged | ineligible — never admitted |
| `head.repo: null` (deleted head branch) | optional-shaped skip-with-event, never a panic |
| already-reviewed SHA | skip, `review_skipped_already_complete` |

### 5.2 `autoreview` label semantics (explicit)

**Phase 1 contract is flag-based**: `auto_review.enabled = true` enables automatic review of every eligible PR revision in watched repositories. The `autoreview` GitHub label is the **canonical reserved label** but is **inert in Phase 1**: no daemon code polls it, no PR eligibility requires it, and it must never be applied to issues as classification. Do not interpret the label as a current eligibility requirement.

---

## 6. Worker execution and the executor boundary

### 6.1 Target-neutral boundary

Worker execution is target-neutral: `WorkTarget::Issue(...)` | `WorkTarget::PullRequest(ReviewTarget)`. The issue-path environment contract (`CADUCEUS_ISSUE_*`) is preserved **byte-for-byte** for user-owned harness compatibility; the PR path adds `CADUCEUS_WORK_TARGET=pr` + `CADUCEUS_PR_*` variables. No synthetic `IssueKey`; no synthetic branch name (review worktrees are detached; the branch-name requirement is lifted for PR targets at the type level).

### 6.2 Mode-aware bridge result contract

The worker bridge must not synthesize a legacy code-fix result for a review run. Result delivery per mode:

| Mode | Result path |
|---|---|
| OCI | `<state_dir>/oci-runs/<run_id>/output/worker-result.json` (off-worktree `/output` mount) |
| TrustedHost | `<worktree>/worker-result.json` (untracked — excluded by the `--untracked-files=no` dirty check) |

For PR targets the bridge resolves **only** the mode-correct path (no legacy-path fallback) and **never synthesizes** a result: a review run that wrote no result file is an execution failure.

### 6.3 OCI required for review in Phase 1

**Auto Review requires OCI execution in Phase 1.** `auto_review.enabled = true` with `executor_mode: trusted_host` fails safely at config validation with an actionable error naming both required changes (`executor_mode: oci` and a valid `sandbox:` section) and pointing at `caduceus doctor` / config docs. TrustedHost offers no containment story for executing third-party tooling against untrusted PR content; pretending otherwise would misstate the security boundary. This is a deliberate behaviour change for default (TrustedHost) installs and is called out in release notes.

### 6.4 Sandbox boundary (the real guarantees)

The review sandbox is configuration of the existing OCI primitive: digest-pinned image, `--read-only` rootfs, `--cap-drop ALL`, `no-new-privileges`, closed two-mode `NetworkMode` (no host networking), tmpfs only at `/tmp`/`/dev/shm`, `.git` as a **read-only daemon-owned shadow**. `/workspace` is hard-coded read-write and structurally cannot be made read-only.

**The accurate security model:**

- **Git metadata/refs**: mutation prevented by the RO `.git` shadow (no commit/push/branch-switch from inside the container).
- **Tracked source files**: technically writable by the worker — a review run **rejects tracked-file mutation before accepting the result** (post-run check, §10.1), it does not prevent the write.
- **Daemon control files**: `worker-prompt.md` is forbidden (prompt instruction) and verified by pre/post integrity check because the untracked-ignoring dirty check would miss it (§10.2).
- **Allowed worker output**: the result file only (per §6.2 paths). Build artefacts from repo tooling are tolerated as untracked noise, never as tracked mutations.

Security tests must prove **these** guarantees — not stronger imaginary ones.

---

## 7. Worker prompt contract

Sectioned, trust-separated, in fixed order (section-ordering is deterministically testable):

1. Daemon instructions & review policy (trusted).
2. Output schema (trusted) — **rendered from the daemon's accepted `ReviewResult.schema_version`** (the daemon injects it; a worker harness ahead of the daemon cannot produce results the daemon rejects).
3. PR metadata: number, title, body (UNTRUSTED), author, base ref/SHA, head ref/SHA, draft flag.
4. Diff: structured file list with hunks over the **merge-base form** (§2.2).
5. Repository context: per-file excerpts (UNTRUSTED, labelled).
6. PR discussion: bounded, deterministically sampled comment window (UNTRUSTED).

No repository file, PR text, or comment may change worker permissions, output schema, mutation policy, GitHub access, sandbox rules, or verdict semantics. `worker-prompt.md` is untouchable by instruction and verified by integrity check (§10.2).

### 7.1 Large-PR budgets

Each untrusted section has a deterministic byte budget, and the prompt has a total budget (existing hard prompt maximum applies). When a PR exceeds budgets: deterministic truncation/sampling (documented per section), and the run proceeds as a **bounded review** — or, if the diff alone exceeds the budget, the run is **skipped with a structured oversized-PR event**. An oversized PR must **not** repeatedly consume the normal worker retry budget: retrying cannot change deterministically-unreviewable input. Large-PR behaviour is deterministically tested.

---

## 8. Execution status vs verdict; retry semantics

**Mandatory separation** (original spec §12):

- `status: success | failure` — did the review execute? Drives retry.
- `verdict: pass | fail` — did the code pass? Drives publication only.

A failed code review is **never** `status: failure`. Verdict consistency is validated: blocking > 0 → FAIL required; blocking = 0 → PASS; **FAIL with zero blocking is rejected as inconsistent** (execution failure); PASS with blocking is rejected; malformed severity/path/line is rejected. All validator rejections are execution failures (`FailureClass::Worker`) and burn normal retry budget — correct, because a retry can produce a valid result.

### 8.1 Terminal vs skip (the two non-retry end states)

| Condition | Class | Outcome |
|---|---|---|
| Mutation violation (tracked-file change detected post-run) | `Terminal` | `finish_needs_attention` + `blocked_recovery_hint` (points at archived worktree) + `review_mutation_violation`. Retry budget **not** burned. |
| Unavailable head SHA (force-push + GC between discovery and execution) | distinct skip route | `finish_skip` (quiet: teardown, claim release) + `review_skipped_head_sha_unavailable`. Self-resolving — the successor SHA is admitted by the next poll. **Not** NeedsAttention. |
| Oversized PR (deterministically unreviewable) | skip route | `finish_skip` + oversized-PR event. Not retry. |

These are three different end states; conflating them either burns retry budget on unfixable conditions or floods NeedsAttention with self-resolving events.

---

## 9. Finalization and publication

### 9.1 Finalizer FSM

`Pending → Publishing → Published | FailedRetryable`, stage persisted atomically at each transition. The `ReviewResult` is durable in history **before any GitHub call**. Publication failure → `FailedRetryable` with persisted retry state (`publication_attempt_count`, `next_publish_at`, `last_publish_error`) — **the model is never re-run because publication failed**; resume is idempotent via `ReviewState.sticky_comment_id`.

### 9.2 Sticky comment

One comment per PR, marker `<!-- caduceus-auto-review -->` (no run id — one comment per PR, not per run). Ownership: `sticky_comment_id` is authoritative; marker search is fallback. Renderer is a **deterministic byte-budget renderer**: reserve space for the marker, PASS/FAIL heading, reviewed SHA, stale-revision notice (if head moved), and truncation notice; consume findings deterministically (blocking → warnings → suggestions, stable within severity = persisted order) within the remaining budget. Never front-truncate. Byte limit 65,536. Re-publishing the same result is **byte-identical** (idempotency requirement, tested).

Body identifies the exact reviewed SHA (and previous SHA when applicable); stale results still publish with the reviewed SHA noted.

### 9.3 Gone-state discrimination (four distinct states)

| # | Condition | At finalization | Policy |
|---|---|---|---|
| A | Comment PATCH → 404 | comment deleted by human | marker search (capped pages) → create-new → persist new id; crash between create and id-persist self-heals via marker adoption |
| B | PR lookup → 404 | PR deleted/inaccessible | quiet skip; **never recreate** |
| C | PR closed-unmerged | superseded work | quiet skip + structured event; historical result remains persisted |
| D | PR merged | review of the merged revision | **publish** (highest-value comment), subject to the stale-generation guard (§9.4) |

Discovery-time behaviour for C/D is ineligibility (§5.1); the table above governs finalization-time.

### 9.4 Monotonic publication guard

Multiple revisions may be in flight; completions can arrive out of order (B admitted after A, B finishes first). Rule: **an older generation may persist its historical result but must never update the PR's current presentation.** `ReviewState.review_generation` (monotonic per `(repo, pr)`, assigned at admission) is the ordering guard: the finalizer compares the completing run's generation against the current generation before any sticky-comment update. Suppressed publications emit a structured event. Historical rows are always written regardless. Tested: B-completes-before-A leaves the sticky comment showing B's result with A's result in history.

---

## 10. Enforcement checks

### 10.1 Post-run tracked-file dirty check

Run after worker exit, before result acceptance, in the review worktree, **with the repository's own git config** (autocrlf/eol): `git status --porcelain --untracked-files=no` (equivalently `git diff --quiet HEAD` — the `.git` shadow keeps index == HEAD). Tracked-file modification = mutation violation → §8.1 Terminal row. Untracked build artefacts are not violations. The result file (TrustedHost `<worktree>/worker-result.json`) is untracked and naturally excluded.

### 10.2 Daemon control-file integrity

`worker-prompt.md` (and any daemon-owned review control files) get an explicit **pre/post integrity check** (hash comparison) because the dirty check deliberately ignores untracked files. Three-way separation enforced and tested: allowed worker output (result file) / build artefacts (untracked noise) / forbidden daemon control files.

### 10.3 Result validation caps

ReviewResult string fields and findings carry validation caps so no single adversarial finding can exceed the comment byte budget by itself (§9.2 renderer still guarantees the invariant independently).

---

## 11. Security model

### 11.1 Trust boundaries

Review workers inspect untrusted content (PR source, diffs, titles, bodies, discussion, repository docs). The sandbox boundary (§6.4) plus the prompt contract (§7) plus the post-run enforcement (§10) together define the defence. Review security posture is at least as strict as Auto Fix, and stricter in effect because PR review naturally executes third-party code.

### 11.2 Fork policy (Phase 1)

Forks are **unsupported**: a first-class, unit-tested predicate (`head.repo.full_name != base.repo.full_name`) gates discovery; `head.repo: null` is optional-shaped. No config knob exists in Phase 1 (dead config would advertise a posture the system cannot deliver — the single-origin mirror cannot checkout fork SHAs). Phase 2 fork review is a **quarantine remote fetch story** (per-PR ephemeral remotes or a quarantined mirror), never a second remote on the persistent daemon mirror.

### 11.3 Prompt injection

Structural escaping (existing prompt machinery) covers all untrusted sections; the adversarial corpus tests (prompt, diff, repo content, discussion vectors) certify that no injection can change permissions, schema, mutation policy, GitHub access, sandbox rules, or verdict semantics.

---

## 12. Investigation migration (releases N and N+1)

**Release N**: ship Review; deprecate Investigation — still works, warns at admission (`ticket_label_investigation` deprecated-warning at config load; transitional label `autofix-investigate`, the deployed plain label — never mapped to `autoreview`); v7→v8 + JSON-envelope bumps are structural; in-flight investigation entries drain through the normal runtime loop.

**Release N+1**: remove the active investigation path (admission, dispatch, finalization). **Retain** legacy parsing/migration + `TicketType::Investigation` as terminal-never-admitted so a **direct pre-N → N+1 upgrade** consumes older persisted state safely (deleting the variant would hard-fail the whole-store load). Startup reconcile pass (§4.4) terminates stranded rows with archive + audit. `ticket_label_investigation` config field removed with an explicit, documented `from_raw` error. `autofix-investigate` label removed with the feature. Release notes must call out the direct-upgrade safety property for skip-N upgraders.

**Trigger labels**: canonical `autofix` (fix dispatch) and `autoreview` (reserved, inert in Phase 1). No emoji-based default remains anywhere after release N. Legacy emoji config values, if explicitly set by an operator, translate at read time with a one-time notice.

---

## 13. Observability

Structured events (each its own log line; owners noted):

```
review_discovered / review_skipped_already_complete / review_skipped_draft      (discovery)
review_skipped_fork_unsupported                                                 (fork gate)
review_admitted                                                                 (discovery)
review_started / review_worker_completed / review_retry_scheduled               (dispatch)
review_execution_failed        ← infrastructure retry path                      (dispatch)
review_passed / review_failed_verdict   ← deliberately distinct words           (dispatch, on validated result)
review_mutation_violation      ← Terminal → NeedsAttention                      (enforcement)
review_skipped_head_sha_unavailable                                            (dispatch skip route)
review_skipped_oversized_pr                                                    (prompt budget)
review_publish_started / review_published / review_publish_failed_retryable    (finalizer)
review_publication_suppressed_stale_generation                                 (monotonic guard)
review_stale_sha_observed                                                      (poll on moved PR)
review_migration_terminated_investigation                                      (N+1 reconcile)
```

`verdict FAIL` and `execution FAILED` must never be conflatable in logs. CLI: `caduceus review status [repo] | list | show <repo> <pr> [--json]` reading the review stores; per-row fields: repo, PR, base/head SHA, merge base, review state, run id, generation, attempts (queue-owned), execution status, verdict, last error, review timestamp, publication state + attempt count + next attempt. JSON output follows the existing envelope-versioning pattern.

---

## 14. Concurrency

Per-`ReviewTarget` claim uniqueness (one claim; losers re-enqueue cleanly). Reviews and fixes share the worker pool under `max_reviews_per_tick` + shared `worker_timeout_seconds` (ADR: one timeout knob in Phase 1; the budget guard is the flood control). Out-of-order completion is handled by the monotonic publication guard (§9.4). Daemon restart mid-review: existing OCI startup recovery + stale-claim handling apply unchanged; finalizer FSM resumes from persisted stage.

---

## 15. Testing strategy

Areas and canonical homes (issue numbers in the epic):

- **Polling** (#312): discovery, draft/fork/closed skip, new-SHA detection, already-reviewed skip, pagination, malformed responses, rate limits, per-repo failure isolation, PR closed between discovery and execution.
- **State/persistence** (#295, #293, #327): durability across restart, dedup, both backends (JSON + SQLite), both envelope bumps, corrupted-state rejection, frozen pre-N and N-era v8 fixtures for both upgrade paths.
- **Worktree/mirror** (#297, #299): exact-SHA checkout, base/merge-base preservation, no branch artefacts, unavailable-SHA rejection, GC.
- **Prompt/result** (#303, #305): section ordering, schema-version injection, adversarial escaping, verdict-consistency matrix (incl. FAIL+0-blocking rejected), malformed-field rejection, caps, large-PR determinism.
- **Finalization** (#308, #310): create/update/idempotent re-publish, byte-identical determinism, four gone-states, marker survival under byte-budget overflow, crash-after-publish-before-mark, stale-generation suppression.
- **Concurrency/crash** (#314): double-claim, B-before-A, restart recovery, shared-pool coexistence.
- **End-to-end** (#322): full lifecycle with no stubs, including stale-generation suppression and FAIL→PASS sticky replacement.
- **Security** (#324): sandbox assertions proving the §6.4 boundary (not more), adversarial corpus, fork predicate incl. null head-repo, mutation/control-file checks (§10).
- **Executor boundary** (#346): env contracts byte-identical (issue path), PR-path vars, bridge mode-awareness, no legacy synthesis.

---

## 16. Phase-1 acceptance gate

The gate (#333) enumerates exactly the canonical Phase-1 issues (no numeric ranges; the N+1 removal issue is **excluded** — it ships in N+1 and does not block the Phase-1 gate). Gate criteria: all Phase-1 children closed; original spec §35 criteria green; E2E test green; frozen-fixture upgrade tests green (both paths); security/observability/docs sign-offs recorded.

---

## 17. Phase-2 extension seams

- Same-SHA re-review via `/caduceus review` (trusted-comment matcher is new logic on the allowlist model; appends history rows — no schema change needed, §4.3).
- Fork trust policy + `allow_fork_prs`-style opt-in + quarantine fetch (§11.2).
- GitHub App webhook transport, Checks API, line annotations, branch-protection integration, re-run controls — all consume the same `ReviewTarget` → engine → `ReviewResult` boundary; presentation is adapter-shaped.
- Coalescing of intermediate revisions (requires a safety proof).
- Review-specific timeout knob (only on POC evidence).

---

## 18. Traceability

Epic: #290. Canonical issue tree: #290–#339 open set + #346 (AR-026). Phase-2 trackers: #335, #337. N+1 tracker: #331. Duplicates closed: #294, #296, #298, #300, #302, #304, #307, #309, #311, #313, #315, #317, #319, #321, #323, #326, #328, #330, #332, #334, #336, #338, #340, #341, #342, #343, #344, #345.
