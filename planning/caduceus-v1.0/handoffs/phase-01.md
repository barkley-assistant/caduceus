# Phase 01 Gate — Baseline CI and test infrastructure

- Phase: 1 (Baseline CI and test infrastructure)
- Outcome: complete
- Files changed: (none in this handoff; the gate is the four task handoffs)
- Phase acceptance IDs: PHASE-01-AC-01, PHASE-01-AC-02, PHASE-01-AC-03, PHASE-01-AC-04, PHASE-01-AC-05, PHASE-01-AC-06
- Commands run:
  - `cargo fmt --check` (no diff)
  - `cargo clippy --locked --all-targets -- -D warnings` (green)
  - `cargo test --locked --all-targets` (all Rust tests pass across all binaries)
  - `python3 -m pytest tests/ -q` (82 passed — 65 existing + 17 new fixture tests)
  - `python3 -B planning/caduceus-v1.0/tools/validate_plan.py` (plan valid: 42 tasks, 8 phases, acyclic and phase-safe)
  - `python3 -B planning/caduceus-v1.0/tools/check_commit_messages.py --range bdd48b8..origin/main` (PASS: all 12 commits since phase 0 follow Conventional Commits with non-empty lowercase scope)
  - `sha256sum planning/caduceus-v1.0/CONTRACTS.md` (059d00ca586f5fa57ddf6959eedd53d3de230a282b3a7de6ccd1fd1226476716 — unchanged from manifest-pinned contracts_sha256)
  - `git status planning/caduceus-v0.1/` (clean; archive untouched)
  - `test -d prompts/ && echo WARNING || echo OK` (no prompts/ directory)
  - `gh pr merge 4` (squash-merged to main, CI green across all 7 jobs)
- Results: every phase acceptance ID below passed; all four Phase 01 tasks are complete with handoffs; the controller returns the Phase 01 gate as the next work item; the CI matrix (rust-1.97, rust-stable, python, planning) ran green on the most recent main commit; 82 Python tests and all Rust tests pass.
- Forbidden-side-effect checks:
  - v0.1 archive untouched; git status planning/caduceus-v0.1/ reports no changes
  - progress.json records each Phase 01 task complete with its handoff path; the gate itself transitions from pending to complete after this handoff
  - no production source modified; all new code lives under tests/ or .github/
  - no prompts/ directory created
  - no daemon-owned state files touched
  - CONTRACTS.md is unchanged (sha256 matches); task-manifest.json is unchanged
- Residual risks:
  - The 1.4 HermesHostFixture self-tests exercise against `/nonexistent/hermes` to prove FileNotFoundError handling; real Hermes binary gated tests are skipif-gated and deferred to Phase 02
  - ProcessTree (1.3) is Linux-only via `#[cfg(target_os = "linux")]`; non-Linux platforms get compile-time stubs
  - CrashPoint (1.3) requires /bin/bash; CI runner (ubuntu-24.04) has it, minimal containers may not
  - The CI workflow ran on the PR merge commit; the fixup commit on main (101ed6c) only touched the handoff markdown — no code changes — and the commit-policy workflow validated it independently
- Blocker evidence (blocked only): n/a, gate is complete

## Phase acceptance evidence

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| PHASE-01-AC-01 | PASS | `cargo test --locked --all-targets` on rust-1.97 and rust-stable; `gh run list --branch main --limit 3 --json conclusion` | rust-1.97 job passed (6m25s), rust-stable job passed (3m8s), all 757 Rust tests green across both toolchains; CI matrix enforced by `.github/workflows/ci.yml` with toolchain pins 1.97.0 and stable | planning/caduceus-v1.0/handoffs/1.1.md and `.github/workflows/ci.yml` |
| PHASE-01-AC-02 | PASS | `cargo test --test fixtures_self_test` (21 tests); `pytest tests/fixtures_hermes_host_self_test.py -q` (9 tests); `pytest tests/hermes_host_test.py -q` (8 tests) | All 38 fixture self-tests pass: 21 Rust (9 from 1.2 + 12 from 1.3) + 17 Python (9 fixture self-tests + 8 capability tests). All fixtures are hermetic — no production credentials, no network, no real `~/.hermes` | planning/caduceus-v1.0/handoffs/1.2.md, planning/caduceus-v1.0/handoffs/1.3.md, planning/caduceus-v1.0/handoffs/1.4.md |
| PHASE-01-AC-03 | PASS | `python3 -B planning/caduceus-v1.0/tools/check_commit_messages.py --range bdd48b8..origin/main` | `check_commit_messages: PASS` — all 12 commits since phase 0 follow Conventional Commits 1.0.0 with non-empty lowercase scope, imperative description, no trailing period, subject ≤80 chars. Examples: `feat(fixtures): ...`, `test(fixtures): ...`, `fix(handoff): ...`, `ci(plan): ...`, `docs(plan): ...` | planning/caduceus-v1.0/tools/check_commit_messages.py and `.github/workflows/commit-policy.yml` |
| PHASE-01-AC-04 | PASS | `grep -rn 'gateway' tests/fixtures/hermes_host.py`; `pytest tests/fixtures_hermes_host_self_test.py -q`; `pytest tests/hermes_host_test.py -q` | The HermesHostFixture (1.4) uses an isolated `$HERMES_HOME` temp directory, never touches `~/.hermes`; `install_cron_capability` simulates 7 capabilities (well_formed, malformed, denied, timed_out, eof, crashed, absent) via `_runtime.install_dispatcher()`; no `gateway start` or `gateway stop` invocations found; AC-03 gateway restart is recorded as an explicit prerequisite evidence row | planning/caduceus-v1.0/handoffs/1.4.md and tests/fixtures/hermes_host.py |
| PHASE-01-AC-05 | PASS | `gh run view --repo barkley-assistant/caduceus $(gh run list --branch main --limit 1 --json databaseId --jq '.[0].databaseId') --json jobs`; `pytest tests/ -q`; `python3 -B planning/caduceus-v1.0/tools/validate_plan.py` | CI on main: all 7 jobs green (ci/rust-1.97, ci/rust-stable, ci/python, ci/planning, commit-policy, CodeQL, Analyze). Local: 82 Python tests green, 757 Rust tests green, plan valid (42 tasks, 8 phases). Walking skeleton report at `planning/caduceus-v1.0/handoffs/1.4-walking-skeleton.md` records every command, exit code, structured category, artifact, and owned expected gap | planning/caduceus-v1.0/handoffs/1.4-walking-skeleton.md |
| PHASE-01-AC-06 | PASS | `pytest tests/ -q`; `grep -rn 'deferred' planning/caduceus-v1.0/handoffs/1.4.md`; `grep -rn 'stubbed' planning/caduceus-v1.0/handoffs/1.4.md`; `grep -rn 'failed' planning/caduceus-v1.0/handoffs/1.4.md`; `grep -rn 'contradicted' planning/caduceus-v1.0/handoffs/1.4.md` | 82/82 tests pass. No silent success — the walking skeleton at `1.4-walking-skeleton.md` documents 3 deterministic Phase 02 gaps (G-02, G-08, G-09) owned by tasks 2.1 and 2.2, plus 2 self-test gaps (G-26, G-27). An unexpected fixture failure (e.g. missing Hermes binary on a non-CI host) is caught by skipif-gated tests; a silent success is caught by the walking skeleton's evidence table | planning/caduceus-v1.0/handoffs/1.4-walking-skeleton.md gap register |