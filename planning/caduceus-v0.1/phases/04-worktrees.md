# Phase 04: Repository and worktree lifecycle

Entry: repository identity and git runner dependencies complete. Exit: clone validation, daemon-owned branches/worktrees, and teardown are safe and idempotent.

## Tasks

- [Task 4.1: Discover and validate local clones](../tasks/4.1-discover-and-validate-local-clones.md)
- [Task 4.2: Create a daemon-owned worktree and branch](../tasks/4.2-create-a-daemon-owned-worktree-and-branch.md)
- [Task 4.3: Tear down safely](../tasks/4.3-tear-down-safely.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo test --locked --test repository_discovery_test --test worktree_create_test --test worktree_remove_test`
- `cargo test --locked --all-targets`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-04.md`.
