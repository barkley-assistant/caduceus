# Attachment 5 — Requirement → Acceptance Evidence

Every requirement in `CONTRACTS.md` is mapped to its acceptance ID
and the current evidence state. This is the contract-side audit that
the operator reads to know what is proven today vs. what is on the
v1.0 plan. Satisfies **0.1-AC-05**.

The evidence state of each acceptance ID is one of:

- `proof-ready` — the production code path and a real test exist
  today.
- `planned` — the acceptance ID is in the v1.0 plan; no production
  code ships it yet.
- `contradicted` — shipped code disagrees with the requirement;
  the gap is owned by the named task.

The full requirement → acceptance-ID list comes from
`task-manifest.json` `requirement_map`. The validator asserts
the map exactly covers every requirement ID in `CONTRACTS.md`.

## PLAN family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `PLAN-001` | `0.1-AC-01` | Draft catalog safety | proof-ready | — (catalog is `active`; controller refuses transitions while `draft`) |
| `PLAN-002` | `0.2-AC-01`, `0.2-AC-02` | Acceptance evidence | planned | 0.2 |
| `PLAN-003` | `0.2-AC-03` | Independent review | planned | 0.2 |
| `PLAN-004` | `0.4-AC-02` | Sealed historical implementation tree | proof-ready | — (manifest digest matches `fc13dd96…d1b3eb`; validator refuses any drift) |
| `PLAN-005` | `0.1-AC-04`–`0.1-AC-06`, `0.1-AC-09` | Public readiness audit | proof-ready (this attachment) | — |

## CI family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `CI-001` | `1.1-AC-01`..`1.1-AC-04` | Continuous integration first | planned | 1.1 |
| `CI-002` | `1.2-AC-01`, `1.3-AC-04`, `1.4-AC-01`..`1.4-AC-10` | Reusable system fixtures | planned | 1.2, 1.3, 1.4 |
| `CI-003` | `1.1-AC-05` | Commit policy | planned | 1.1 |

## INSTALL family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `INSTALL-001` | `2.1-AC-01` | Production configuration bootstrap | planned | 2.1 |

## HERMES family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `HERMES-001` | `2.2-AC-01` | Transactional scheduling | planned | 2.2 |
| `HERMES-002` | `2.2-AC-02` | Diagnosable host health | planned | 2.2 |

## QUALITY family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `QUALITY-001` | shared audit, owned by 0.1 + 2.x + 7.x | Shipped integrations are production-ready | proof-ready for v0.1 baseline (zero `todo!()` / `unimplemented!()`); v1.0 enforcement via installed-path verification in Task 7.5 + planning-language scanner per task | 0.1 (audit), 2.x (corrections), 7.5 (lifecycle) |

## RUN family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `RUN-001` | `2.3-AC-01` | One production worker path | planned | 2.3 |
| `RUN-002` | `2.4-AC-01` | Deadline + process-tree cleanup | planned | 2.4 |
| `RUN-003` | `2.5-AC-01` | Bounded transcripts | planned | 2.5 |
| `RUN-004` | `2.6-AC-01` | Hardened Git execution + ephemeral credential transport | planned | 2.6 |
| `RUN-005` | `2.7-AC-01` | CLI status codes | contradicted | 2.7 (`DEBT-STATUS`) |

## STATE family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `STATE-001` | `3.2-AC-01` | SQLite is the v1.0 runtime store | planned | 3.2 |
| `STATE-002` | `3.3-AC-01` | Explicit safe migration | planned | 3.3 (v1.0 `--to sqlite`; v0.1 `--from <legacy.json>` remains the shipped form) |
| `STATE-003` | `3.4-AC-01` | Supported recovery commands | planned | 3.4 (human review) |
| `STATE-004` | `3.5-AC-01`, `3.6-AC-01` | Generations + retention | planned | 3.5, 3.6 |

## FINAL family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `FINAL-001` | `4.1-AC-01`, `4.2-AC-01` | Durable checkpoints | planned | 4.1, 4.2 |
| `FINAL-002` | `4.3-AC-01` | Human merge lifecycle | proof-ready (no auto-merge today; v1.0 adds the close-without-merge → NeedsAttention path in 4.2) | — |

## SCHED family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `SCHED-001` | `5.1-AC-01`, `5.2-AC-01` | Bounded single-host concurrency | planned | 5.1, 5.2 |
| `SCHED-002` | `5.3-AC-01` | Failure control | planned | 5.3 |

## REPO family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `REPO-001` | `5.4-AC-01` | Daemon-owned repositories | planned | 5.4 |

## GH family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `GH-001` | `5.5-AC-01`, `5.5-AC-05` | Authentication and discovery (incl. endpoint allowlist) | proof-ready for PAT + ETags; endpoint allowlist is planned | 5.5 |

## EXEC family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `EXEC-001` | `6.1-AC-01`, `6.2-AC-01` | Executor abstraction | planned | 6.1, 6.2 |
| `EXEC-002` | `6.3-AC-01`, `6.4-AC-01` | Isolation defaults and boundary | planned | 6.3, 6.4 (human review) |

## CONFIG family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `CONFIG-001` | `3.7-AC-01` | Configuration schema v2 | planned | 3.7 |
| `CONFIG-002` | (cross-cutting; covered by every Rust acceptance ID) | Toolchain (Rust 2021, MSRV 1.97) | proof-ready | — (`Cargo.toml` `rust-version = "1.97"`, `Cargo.lock` committed, every CI job runs `--locked`) |

## ACCEPT family

| Requirement | Acceptance ID | Title | State | Owner |
|---|---|---|---|---|
| `ACCEPT-001` | `7.1-AC-01`, `7.2-AC-01` | Full-system regression suite | planned | 7.1, 7.2 |
| `ACCEPT-002` | `7.3-AC-01` | Host and release evidence | planned | 7.3 |
| `ACCEPT-003` | `7.5-AC-01` | Installed-path truth | planned | 7.5 (human review) |

## Summary

| State | Count | Notes |
|---|---|---|
| `proof-ready` | 7 | `PLAN-001`, `PLAN-004`, `PLAN-005`, `QUALITY-001` (baseline), `FINAL-002`, `GH-001` (PAT + ETag subset), `CONFIG-002` |
| `contradicted` | 1 | `RUN-005` (`DEBT-STATUS`, owned by 2.7) |
| `planned` | 30 | the rest; each has a single named v1.0 owner |
| **total** | **38** | matches `len(set(requirement_map.keys()))` |

## Reproduction

```bash
# Requirement map and acceptance IDs
python3 -c "
import json
m = json.load(open('planning/caduceus-v1.0/task-manifest.json'))
for rid, accepted in m['requirement_map'].items():
    print(rid, '->', accepted)
"

# Requirement IDs in CONTRACTS.md
grep -nE "^### [A-Z]+-[0-9]{3} —" planning/caduceus-v1.0/CONTRACTS.md

# Validator cross-check (covers the requirement-map coverage rule)
python3 -B planning/caduceus-v1.0/tools/validate_plan.py
```