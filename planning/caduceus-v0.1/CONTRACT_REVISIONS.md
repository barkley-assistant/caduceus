# Contract revisions

This is the approval record for changes to the sealed cross-task contract. It is not a task handoff. Each entry must identify the approving reviewer, rationale, affected work, and re-verification required before the contract digest is updated.

## CR-002 — 2026-07-13 — MSRV bump to Rust 1.97

- Approver: project reviewer (user-authorized out-of-band)
- Rationale: Rust 1.97 is the current stable toolchain as of 2026-07-13. The previously pinned MSRV 1.75 is two years old and predates several language and standard-library improvements used in modern async, error-handling, and serialization code. Bumping to 1.97 unblocks adoption of newer idioms without raising individual dependency MSRVs above what 1.97 already requires.
- Affected contract surfaces:
  - `CONTRACTS.md` Toolchain section: `Rust 2021, MSRV 1.75` → `Rust 2021, MSRV 1.97`
  - `CONTRACTS.md` Toolchain section: "must pass the release suite on Rust 1.75" → "must pass the release suite on Rust 1.97"
- Affected task packets: 0.1 (sets `Cargo.toml` `rust-version`); 9.2 (release gate includes `cargo +1.97 ...` checks).
- Affected phase gates: 00-scaffolding, 09-release (both already invoke `cargo --locked`, which now resolves against 1.97).
- Required re-verification:
  - Plan validator passes with refreshed `contracts_sha256`.
  - Task 0.1 sets `rust-version = "1.97"` in `[package]`.
  - `cargo +1.97 build --locked --all-targets` succeeds from a clean clone.
  - `cargo +1.97 clippy --locked --all-targets -- -D warnings` is the gate for every task completion.
  - Phase 09 release gate runs the full test suite on 1.97 and records the result in `handoffs/phase-09.md`.
- Archive: unchanged; `archive/full-reviewed-plan.md` remains immutable.

## CR-001 — 2026-07-13 — approved plan-control clarification

- Approver: project reviewer
- Rationale: make contract-drift handling, inbound comment filtering, outbound public-voice matching, legacy Hermes migration, worker-result retry semantics, and the process-supervisor review checkpoint unambiguous before implementation.
- Affected task packets: 0.2, 1.1, 5.1, 5.3, 5.6, 6.6, 7.1.
- Affected phase gate: 05-workers.
- Required re-verification: plan validator; controller rejection of Task 5.1 completion without its human-review artifact; structural and matching tests named in the affected packets.
- Archive: unchanged; `archive/full-reviewed-plan.md` remains immutable.
