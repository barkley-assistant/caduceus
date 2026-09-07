# Adversarial corpus fixtures (issue #324, DAR §11.3)

Each `.json` file is one corpus case consumed by
`tests/security/corpus_test.rs`. The harness feeds the payload
through `build_review_prompt` and asserts the prompt's structural
invariants survive: unchanged fence count, trusted policy section
verbatim and before all untrusted content, untrusted-data marker
present, daemon's `schema_version` intact, and no second policy
header.

## Schema

| Field | Type | Notes |
|---|---|---|
| `label` | string | short label for assert panics |
| `targets` | string[] | which untrusted fields the vector attacks |
| `payload` | object | optional `pr_title`, `pr_body`, `diff`, `repo_context`, `discussion`; missing = benign default |
| `expect_oversized` | bool\|null | `true` = assert `Oversized`; `false`/null = assert `Bounded` |

## Vector categories

- `01-08`: schema escape + verdict manipulation
- `09-14`: mutation-policy + GitHub-access + sandbox-escape

Payloads are deliberately small (<4 KiB): the corpus targets
invariant preservation, not budget behaviour (budgets are covered
by `tests/worker/review_prompt_test.rs`).

REALITY CLAUSE (DAR §6.4): nothing here asserts the workspace is
read-only or that mutation is impossible. The review posture is a
read-write `/workspace` with a post-run tracked-file dirty check
(#306) plus the read-only `.git` shadow.
