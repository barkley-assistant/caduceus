# Phase 05: Worker context and supervision

Entry: worktree and issue-detail contracts complete. Exit: sanitized environment, bounded context/prompt, worker result parsing, supervisor timeout, parent-death cleanup, transcript, and heartbeat behavior pass.

## Tasks

- [Task 5.2: Construct a deny-by-default worker environment](../tasks/5.2-construct-a-deny-by-default-worker-environment.md)
- [Task 5.6: Build stable context JSON](../tasks/5.6-build-stable-context-json.md)
- [Task 4.4: Generate the canonical prompt file](../tasks/4.4-generate-the-canonical-prompt-file.md)
- [Task 5.1: Spawn and supervise the entire worker process tree](../tasks/5.1-spawn-and-supervise-the-entire-worker-process-tree.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo test --locked --test worker_env_test --test context_test --test prompt_test --test worker_process_test --test worker_parent_death_test`
- `cargo test --locked --all-targets`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Task 5.1 additionally requires the human-authored `../handoffs/5.1-human-review.md` approval artifact; review the Linux descendant/daemon-death tests and supervisor implementation before this gate. Record results in `../handoffs/phase-05.md`.
