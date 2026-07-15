# Phase 00 Gate — Specification and plan integrity

- Phase: 0 (Specification and plan integrity)
- Outcome: complete
- Files changed: (none in this handoff; the gate is the four task handoffs + the six readiness attachments)
- Phase acceptance IDs: PHASE-00-AC-01, PHASE-00-AC-02, PHASE-00-AC-03, PHASE-00-AC-04
- Commands run:
  - python3 -B planning/caduceus-v1.0/tools/validate_plan.py (plan valid: 42 tasks, 8 phases, acyclic and phase-safe)
  - python3 -B planning/caduceus-v1.0/tools/test_set_status_negatives.py (PASS: all 12 negative tests)
  - python3 -B planning/caduceus-v1.0/tools/test_seal_negatives.py (PASS: all 8 negative tests)
  - sha256sum planning/caduceus-v1.0/CONTRACTS.md (matches task-manifest.json contracts_sha256)
  - python3 -c 'import hashlib,pathlib; ...' (v0.1 tree digest matches task-manifest.json v01_tree_sha256)
- Results: every phase acceptance ID below passed; the v0.1 archive is byte-for-byte unchanged; the contract digest still matches; the controller returns the first Phase 01 task as the next item.
- Forbidden-side-effect checks:
  - v0.1 archive untouched; git status planning/caduceus-v0.1/ reports no changes
  - no production source modified
  - progress.json records each phase-0 task complete with its handoff path; the gate itself is now in_progress and will move to complete after this handoff
- Residual risks:
  - 41 v1.0 tasks remain pending; the gap register (06-gap-register.md) routes each open gap to a single Phase 01-07 owner
  - the v0.1 archive still contains pre-CR-002 references to Rust 1.75; the v1.0 historical handoff (0.3-msrv-historical-deviation.md) and the v0.1 revision log (CR-002) jointly close DEBT-MSRV without editing the archive
- Blocker evidence (blocked only): n/a, gate is complete

## Phase acceptance evidence

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| PHASE-00-AC-01 | PASS | python3 -B planning/caduceus-v1.0/tools/validate_plan.py; sha256sum planning/caduceus-v1.0/CONTRACTS.md; grep contracts_sha256 planning/caduceus-v1.0/task-manifest.json; python3 -c 'import hashlib,pathlib; ...' (recompute v0.1 tree digest) | Catalog is active (42 tasks, 8 phases, acyclic, phase-safe); the contract digest in task-manifest.json matches the live CONTRACTS.md sha256; the v0.1 tree digest matches task-manifest.json v01_tree_sha256; the evidence rules enforced by set_status.py and validate_plan.py (Task 0.2 + 0.4) are exercised by the two negative-test drivers (12 + 8 tests, all green) | planning/caduceus-v1.0/handoffs/0.2.md and planning/caduceus-v1.0/handoffs/0.4.md and the two negative-test drivers |
| PHASE-00-AC-02 | PASS | ls planning/caduceus-v1.0/handoffs/readiness/; grep -nE '\\.\\./' planning/caduceus-v1.0/handoffs/readiness/*.md; grep -nE '\\]\\(' planning/caduceus-v1.0/handoffs/readiness/*.md; python3 -B planning/caduceus-v1.0/tools/validate_plan.py | All six readiness attachments exist under planning/caduceus-v1.0/handoffs/readiness/ (00-INDEX.md, 01-capability-inventory.md, 02-reachability-map.md, 03-operator-journeys.md, 04-fault-injection.md, 05-requirement-evidence.md, 06-gap-register.md); the 00-INDEX.md table cross-links every attachment; the validator's local-link check passes; each attachment ends with a Reproduction block of independent commands that the operator can run | planning/caduceus-v1.0/handoffs/0.1.md and the seven files under planning/caduceus-v1.0/handoffs/readiness/ |
| PHASE-00-AC-03 | PASS | grep -nE '^## G-' planning/caduceus-v1.0/handoffs/readiness/06-gap-register.md; python3 -c 'import json; m=json.load(open("planning/caduceus-v1.0/task-manifest.json")); [print(t["id"], ac) for t in m["tasks"] for ac in t.get("acceptance_ids", [])]' | Every gap row in 06-gap-register.md names exactly one existing task or acceptance owner; no row is "Owner: dash and Status: open"; the 26 gap rows map to 35 v1.0 task / acceptance IDs across all 8 phases; v1.x deferrals (GitHub App auth, webhooks, multi-host, custom bridge composition, native dashboards, automated release tooling) are listed in CONTRACTS.md "Explicit v1.x deferrals" as approved out-of-scope items | planning/caduceus-v1.0/handoffs/readiness/06-gap-register.md |
| PHASE-00-AC-04 | PASS | python3 -B planning/caduceus-v1.0/tools/test_set_status_negatives.py; python3 -B planning/caduceus-v1.0/tools/test_seal_negatives.py; python3 -B planning/caduceus-v1.0/tools/validate_plan.py; sha256sum planning/caduceus-v1.0/CONTRACTS.md; git status --porcelain planning/caduceus-v0.1/ | An independent contributor can reproduce every factual classification: the 12-test set_status negative driver proves evidence enforcement; the 8-test seal negative driver proves the v0.1 tree seal and the activation gate; the live validator confirms the catalog, contract digest, v0.1 seal, and progress parity; the live v0.1 archive is byte-for-byte unchanged; every readiness attachment ends with a Reproduction block of independent commands; the historical MSRV deviation is reproducible through grep against the v0.1 packet, the v0.1 archive, the v0.1 revision log, the live Cargo.toml, and the v1.0 contract | planning/caduceus-v1.0/tools/test_set_status_negatives.py and planning/caduceus-v1.0/tools/test_seal_negatives.py and the Reproduction sections in planning/caduceus-v1.0/handoffs/0.1.md and planning/caduceus-v1.0/handoffs/0.3.md and planning/caduceus-v1.0/handoffs/0.3-msrv-historical-deviation.md |
