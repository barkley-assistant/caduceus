# Handoff phase-01 — Foundational contracts

- Work item: Phase 01 gate (Foundational contracts)
- Outcome: complete
- Date: 2026-07-13

## Phase summary

Phase 01 introduced the canonical cross-cutting contracts that
later phases build on. All eleven tasks completed without
contract revisions, with 237 Rust tests + 30 pytest tests
passing on Rust 1.97.

| Task | Title | Status |
|---|---|---|
| 0.2 | Implement and validate the Hermes adapter | complete |
| 1.1 | Parse and validate Config | complete |
| 1.2 | Resolve GitHub authentication | complete |
| 1.3 | Resolve config files and environment overrides | complete |
| 1.4 | Initialize structured logging safely | complete |
| 1.5 | Implement the unified error hierarchy | complete |
| 1.6 | Validate the worker command and runtime prerequisites | complete |
| 3.0 | Implement validated queue data types | complete |
| 5.3 | Parse and validate worker results | complete |
| 6.6 | Enforce the public-voice rule | complete |
| 7.2 | Persist complete daemon metadata | complete |

## Gate commands run

```
$ python3 -B planning/caduceus-v0.1/tools/validate_plan.py
plan valid: 46 tasks, 10 phases, acyclic and phase-safe

$ cargo test --locked --all-targets
test result: ok. 21 passed (meta_test.rs)
test result: ok. 26 passed (voice_rule_test.rs)
test result: ok. 29 passed (worker_result_test.rs)
test result: ok. 27 passed (queue_model_test.rs)
test result: ok. 20 passed (validate_test.rs)
test result: ok. 16 passed (logging_test.rs)
test result: ok. 20 passed (config_resolution_test.rs)
test result: ok. 34 passed (config_test.rs)
test result: ok. 28 passed (error_test.rs)
test result: ok. 16 passed (token_test.rs)

$ pytest -q tests/hermes_plugin_test.py
30 passed in 0.45s

$ cargo fmt --check
# no diff

$ cargo clippy --locked --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

## Per-task deliverables

Every task has a complete handoff under
`planning/caduceus-v0.1/handoffs/<task-id>.md`:

- `0.2.md` — Hermes adapter
- `1.1.md` — Config parser/validator
- `1.2.md` — GitHub token resolver
- `1.3.md` — Config file + env resolver
- `1.4.md` — Structured logging
- `1.5.md` — Unified error hierarchy
- `1.6.md` — Runtime preflight check
- `3.0.md` — Validated queue data types
- `5.3.md` — Worker result parser/validator
- `6.6.md` — Public-voice rule
- `7.2.md` — Daemon metadata persistence

## Forbidden-side-effect spot-checks

Per `CONTRACTS.md` and the Phase 01 gate:

- `Config` parser never executes the worker process, never
  shells out to git, and never writes to disk. The validation
  pipeline is a pure function over the supplied YAML + env.
- Token resolution runs four sources in order, never logs the
  token, and never persists the token to disk. The
  `error::scrub` redaction layer also catches any future leak.
- Structured logging is initialized exactly once per process;
  re-entrant calls fail fast. The non-blocking writer keeps
  the file stream alive for the lifetime of the daemon's main
  loop.
- The error hierarchy uses a manual `Debug` impl that scrubs
  every string field through `error::scrub`. `Display` is
  caller-scrubbed (callers pre-scrub before constructing an
  error variant).
- Worker-result validation opens the file with `O_NOFOLLOW`,
  verifies the descriptor is a regular file, and reads with a
  1 MiB cap before allocating the full document. All file/schema
  failures are wrapped as `CaduceusError::Worker` with a
  stable context label.
- Public-voice validation routes every legitimate HTTP mutation
  through `check_voice_or_error` before any HTTP layer is
  touched. A rejected request returns `CaduceusError::Other`
  and never reaches the wire.
- Daemon metadata persistence uses an atomic rename strategy
  that never leaves a partial file. Concurrent updates are
  serialised by `MetaStore`'s internal mutex. A corrupt file is
  preserved, copied to a timestamped backup, and tagged with
  `<state_dir>/state_meta.corrupt`.

## CONTRACTS.md status

The contracts file was **not modified** during Phase 01.

```
$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
```

The hash matches the value pinned in
`planning/caduceus-v0.1/task-manifest.json` (`contracts_sha256`).

## Residual risks

- `Config::load()` is still a placeholder for Phase 1.3's
  resolution; Phase 1.3 wired the chain in production paths but
  the public `Config::load()` returns an explicit error until
  Phase 2 wires the daemon's main loop.
- The worker's `O_NOFOLLOW` cap test exercises the helper
  directly. A Phase-5 integration test should validate the
  path through the daemon's tick loop end-to-end.

## Blocker evidence (blocked only)

Not blocked.

## Next

Phase 02 (Polling & GitHub API) is the next phase per the plan
manifest. This handoff closes Phase 01; the next controller
invocation will return `kind: task` for the next phase.