# Auto Review architecture (current)

Short module map for the shipped Auto Review path. Operator how-to:
[docs/auto-review.md](../auto-review.md). Fork trust detail:
[docs/security/fork-trust-posture.md](../security/fork-trust-posture.md).
Historical Phase-1 DAR text lives in git history only.

## Product placement

Caduceus runs one tick (`caduceus run`). Auto Review is an opt-in half of that
tick, gated by `auto_review.enabled` and **requiring** `executor_mode: oci` plus
a digest-pinned `sandbox:`. Ticket implementation (`autofix`) is the other half
and may use TrustedHost or OCI. The `autoreview` GitHub label is reserved and
inert.

## Tick order (review-relevant steps)

From [`src/daemon/tick/mod.rs`](../../src/daemon/tick/mod.rs):

1. Config, lock, state open, cadence / rate gates
2. Reap / GC / retention
3. Discover watched repos; poll labeled issues into the issue queue
4. **5.5** — if Auto Review enabled and not dry-run: discover open PRs, admit
   new head SHAs (eligibility, mirror/quarantine fetch, merge base), bounded by
   `max_reviews_per_tick`
5. **5.55** — scan PR comments for trusted `rerun_command` (default `/caduceus
   review`); enqueue same-SHA re-reviews
6. Drain issue claims through the worker pool
7. **6.5a** — claim review queue entries through the same pool
8. Supervise workers; finalize (issue path or review sticky comment)
9. Persist tick outcome

## Module map

| Area | Path | Role |
| --- | --- | --- |
| Config | `src/infra/config/` | `auto_review`, `sandbox`, OCI/TrustedHost validation |
| Discovery | `src/daemon/tick/review_discovery.rs` | PR eligibility, fork gate, admission |
| Re-run | `src/daemon/tick/review_rerun.rs` | Trusted-comment same-SHA re-review |
| Orchestration | `src/daemon/orchestration/review_run.rs` | Claim → worktree → executor → result |
| Executor / OCI | `src/executor/` | `SandboxSpec`, lifecycle, bridge env |
| Fork quarantine | `src/repo/fork_quarantine.rs`, `src/github/fork_gate.rs` | Opt-in fork review without polluting the mirror |
| Prompt / result | `src/worker/review_prompt.rs`, `worker_contract.rs` | PR env + `ReviewResult` schema |
| Sticky comment | `src/review/` | Publish / update / generation guard |
| State | `src/state/review/` | Review queue + history (JSON or SQLite) |
| CLI | `src/cli/review.rs` | `review status\|list\|show` |

## Contracts worth remembering

- **Identity of a review unit:** `owner/repo` + PR number + head SHA (frozen at
  admission).
- **Diff semantics:** merge-base (three-dot) against the frozen head.
- **Execution status vs verdict:** infrastructure Success/Failure vs Pass/Fail
  code outcome — never conflate (CLI separates them).
- **Sticky comment marker:** `<!-- caduceus-auto-review -->` (generation tagged
  when needed). `publication_mode`: `update` | `new_comment`.
- **Forks:** fail-closed unless base repo is in
  `auto_review.fork_policy.allow_fork_prs`; allowed forks use a throwaway
  quarantine clone.
- **Worker never gets GitHub tokens.** Deny list is hard-coded.

## Related docs

- Operator guide: [../auto-review.md](../auto-review.md)
- CLI: [../cli.md](../cli.md)
- OCI certification:
  [../certification/oci-certification.md](../certification/oci-certification.md)
- Migration (Investigation removal): [../migration.md](../migration.md)
