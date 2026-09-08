# ORIGIN — `n-era-investigation/`

Hand-synthesized fixture for issue #331 (release N+1 startup
reconcile pass, AC3), created 2026-09-08.

This is NOT a real captured store. It models an **N-era v2-envelope
`state.json` carrying one Investigation row admitted during release
N** — the case DAR §4.4 calls out: an operator running release N may
admit Investigation rows after the structural migration, so no
migration step will ever see them; only the N+1 startup reconcile
pass can terminate them.

- Envelope shape copied verbatim from the frozen `n-era/state.json`
  (`{"version":2,"entries":{}}`, #327) — the n-era queue was empty, so
  the investigation row is synthesized.
- The `owner/r#23` entry object mirrors the field shape of the frozen
  `pre-n/state.json` investigation entry (#327), with
  `phase: "in_progress"` (mid-N admission, non-terminal — the phase
  the reconcile pass must catch; `AwaitingReview` is equally
  non-terminal and is covered by the same terminal-set logic).

Companion SQLite coverage is generated in-test (`investigation_removal_test.rs`
INSERTs the row into a copy of the frozen `n-era/state.db`), keeping a
generated binary artefact out of git.
