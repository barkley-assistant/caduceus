# Phase 06: Finalization and maintenance

Entry: worker/worktree core complete. Exit: maintenance and every finalization stage are idempotent, voice-checked, resumable, and covered for forbidden side effects.

## Tasks

- [Task 3.3: Reap stale claims and abandoned worktrees](../tasks/3.3-reap-stale-claims-and-abandoned-worktrees.md)
- [Task 4.5: Implement safe worktree GC](../tasks/4.5-implement-safe-worktree-gc.md)
- [Task 5.0: Define finalization interfaces without runtime stubs](../tasks/5.0-define-finalization-interfaces-without-runtime-stubs.md)
- [Task 5.4: Render artifacts and public PR text safely](../tasks/5.4-render-artifacts-and-public-pr-text-safely.md)
- [Task 5.5: Implement dry-run as a first-class outcome](../tasks/5.5-implement-dry-run-as-a-first-class-outcome.md)
- [Task 6.1: Inspect changes and commit code results](../tasks/6.1-inspect-changes-and-commit-code-results.md)
- [Task 6.2: Push idempotently through git](../tasks/6.2-push-idempotently-through-git.md)
- [Task 6.3: Find or create the pull request](../tasks/6.3-find-or-create-the-pull-request.md)
- [Task 6.4: Post completion and close idempotently](../tasks/6.4-post-completion-and-close-idempotently.md)
- [Task 6.5: Finalize failures and investigations](../tasks/6.5-finalize-failures-and-investigations.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo test --locked --test reaper_test --test worktree_gc_test --test pr_body_test --test dry_run_test --test commit_test --test push_test --test pr_test --test issue_close_test --test failure_investigation_test`
- `cargo test --locked --all-targets`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-06.md`.
