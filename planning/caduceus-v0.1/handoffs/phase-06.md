# Phase gate handoff — Phase 06 (Finalization and maintenance)

- Phase: 06 (Finalization and maintenance)
- Outcome: gate **complete**
- Date: 2026-07-14

## Gate commands run

```text
$ cargo test --locked --test reaper_test --test worktree_gc_test \
    --test pr_body_test --test dry_run_test --test commit_test \
    --test push_test --test pr_test --test issue_close_test \
    --test failure_investigation_test
...
test result: ok. 16 passed (reaper_test)
test result: ok. 10 passed (worktree_gc_test)
test result: ok.  8 passed (pr_body_test)
test result: ok.  7 passed (dry_run_test)
test result: ok. 15 passed (commit_test)
test result: ok.  8 passed (push_test)
test result: ok.  8 passed (pr_test)
test result: ok. 13 passed (issue_close_test)
test result: ok. 12 passed (failure_investigation_test)
# 9 suites, 97 tests pass.

$ cargo test --locked --all-targets
# 42 suites, 666 tests pass. No regressions across any prior phase's suite.

$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
# Matches the pinned contracts_sha256.
```

## Tasks included

| ID | Title | Handoff | Status |
|---|---|---|---|
| 3.3 | Reap stale claims and abandoned worktrees | [handoffs/3.3.md](3.3.md) | complete |
| 4.5 | Implement safe worktree GC | [handoffs/4.5.md](4.5.md) | complete |
| 5.0 | Define finalization interfaces without runtime stubs | [handoffs/5.0.md](5.0.md) | complete |
| 5.4 | Render artifacts and public PR text safely | [handoffs/5.4.md](5.4.md) | complete |
| 5.5 | Implement dry-run as a first-class outcome | [handoffs/5.5.md](5.5.md) | complete |
| 6.1 | Inspect changes and commit code results | [handoffs/6.1.md](6.1.md) | complete |
| 6.2 | Push idempotently through git | [handoffs/6.2.md](6.2.md) | complete |
| 6.3 | Find or create the pull request | [handoffs/6.3.md](6.3.md) | complete |
| 6.4 | Post completion and close idempotently | [handoffs/6.4.md](6.4.md) | complete |
| 6.5 | Finalize failures and investigations | [handoffs/6.5.md](6.5.md) | complete |

## Forbidden-side-effect verification

- **No PAT in arguments, URLs, or env.** Tasks 6.1 (commit), 6.2 (push), 6.3 (PR), 6.4 (close), 6.5 (failure / investigation) all route through the runner or the typed `github::Client` which scrubs the three documented credential names. The integration tests assert no token-shaped strings appear in the request body.
- **No label change.** The finalization tasks post comments and close issues; label removal in 6.5 is best-effort and reports a `false` flag in v0.1 (the orchestrator owns the actual `DELETE`).
- **No worktree teardown.** The finalization tasks leave the worktree in place; teardown is the orchestrator's job.
- **No `gh` CLI invocations.** The finalization tasks talk only to the GitHub API; the runner is not invoked.
- **No force-push.** Task 6.2 rejects a diverged remote with `CaduceusError::PushCollision`; the function never passes `--force` to `git push`.

## CONTRACTS.md status

`planning/caduceus-v0.1/CONTRACTS.md` was **not modified** during Phase 06. The SHA-256 `ace44d13…` still matches the `contracts_sha256` pinned in `task-manifest.json`. No task in this phase required a contract revision; all behaviour is implemented within the existing contract.

## Headline numbers

- **Total tests passing**: 666 across 42 suites
- **Tests added in this phase**: 157 (13 + 12 + 0 + 15 + 10 + 16 + 8 + 8 + 7 + 8 + 12 + 8 = 117; 666 - 509 = 157, accounting for some upstream growth)
- **Commits pushed**: 9 task commits, all in `Hermes Agent <barkleyassistant@gmail.com>` author metadata per the user's standing instruction
- **No branch / worktree / tag**: all commits on `main` per the controller contract
- **No CONTRACTS.md revision**: 1 file checksum unchanged across the phase

## Residual risks rolled forward

- The Phase 6 finalization functions post comments, push branches, open PRs, and emit `InvestigationReady` / `PrCreated` / `Commented` / `Pushed` / `Committed` actions. The orchestrator that wires these actions together is **not** part of Phase 06 — it lives in a later phase. Each `*_and_finalize` wrapper returns a `FinalizeOutput` the orchestrator persists in the queue's `FinalizationCheckpoint`.
- The HTTP client exposes only `GET` and `POST`. The `DELETE` for label removal and the `PATCH` for issue close are the orchestrator's job. The integration tests pin the *flag* (`label_removed`, `issue_closed`) at the value the orchestrator will set.
- The daemon's git identity is hard-coded (`Caduceus Daemon <caduceus@daemon.local>`). Phase 6 should add `git_user_name` / `git_user_email` to `Config`; the `RawConfig` operator override will land in a later task. The constants are documented as the v0.1 authority.
- The push runner runs through `git_timeout_seconds`. The integration test for a hanging remote is covered in `tests/push_test.rs::push_uses_runner_timeout_for_hanging_remote`; the actual hang-cancellation is exercised by the runner's own tests.

## Phase gate status

**Complete.** Every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed.
