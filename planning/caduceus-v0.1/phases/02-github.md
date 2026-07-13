# Phase 02: GitHub client and polling

Entry: foundational types complete. Exit: all GitHub reads use typed schemas, bounded pagination/cache, persisted rate-limit observations, and no Events API polling.

## Tasks

- [Task 2.1: Build the typed HTTP client and persistent conditional cache](../tasks/2.1-build-the-typed-http-client-and-persistent-conditional-cache.md)
- [Task 2.2: Discover watched repositories](../tasks/2.2-discover-watched-repositories.md)
- [Task 2.3: Poll open labeled issues with a typed schema](../tasks/2.3-poll-open-labeled-issues-with-a-typed-schema.md)
- [Task 2.4: Handle poll cadence and rate limits](../tasks/2.4-handle-poll-cadence-and-rate-limits.md)
- [Task 2.5: Verify the selected trigger label immediately before work](../tasks/2.5-verify-the-selected-trigger-label-immediately-before-work.md)
- [Task 2.6: Fetch complete issue detail](../tasks/2.6-fetch-complete-issue-detail.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo test --locked --test github_client_test --test repository_poll_test --test issue_poll_test --test rate_limit_test --test cadence_test --test verify_test --test issue_detail_test`
- `cargo test --locked --all-targets`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-02.md`.
