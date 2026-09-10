# Fork trust posture and quarantine fetch (Phase 2, #337)

Phase 1 hard-gated every fork PR (`head.repo.full_name !=
base.repo.full_name` → `review_skipped_fork_unsupported`). Phase 2
replaces that hard gate with an explicit per-repo trust policy and a
safe fetch story, **without touching the Phase-1 single-origin mirror
path for non-fork PRs** (DAR §11.2, byte-for-byte).

## Trust surface

Reviewing a fork PR means executing attacker-controlled content:

- **Head SHA**: the fork's head commit SHA comes from the wire row.
  Discovery validates the SHA shape (hex, 40 chars) before admission,
  but the *object* it names is entirely fork-owned.
- **Diff**: `git diff <merge_base> <head_sha>` renders fork-authored
  content into the review prompt (merge-base semantics, DAR §2.2).
  The merge base itself is computed inside the quarantine clone, so a
  poisoned base/head pair can only change the *diff scope*, never the
  daemon's instructions.
- **Fork repo metadata**: `head.repo.full_name` (and any fork-side
  ref/identity values) are attacker-controlled. The prompt's identity
  lines render from the daemon-frozen target; wire-row metadata never
  overrides the frozen repository identity.
- **PR body / discussion**: as with same-repo PRs, these render as
  untrusted, fence-escaped sections (§11.3, adversarial corpus
  fixtures `15-17`).

## Quarantine containment

Fork objects never enter the production mirror. The Phase-1 mirror is
single-origin (DAR §11.2) and the quarantine design preserves that
invariant by construction:

1. **Per-PR clone**: admission creates a quarantine clone at
   `<state_dir>/fork-quarantine/<owner>/<repo>/<pr>@<head_sha>/`,
   cloned from the **trusted base repo URL** (the daemon's own
   remote resolution, never a fork-provided URL).
2. **SHA-anchored fetch**: the fork's head SHA is fetched from the
   fork URL with `git fetch <url> <sha>` — no ref/branch artefact is
   written, and an unavailable SHA fails the admission
   (`HeadShaUnavailable` → the row is skipped and retried next poll).
3. **Merge base inside the quarantine**: `git merge-base <base_sha>
   <head_sha>` runs against the quarantine clone, so the persisted
   merge base always has both parents present locally.
4. **No second remote**: the quarantine clone is a throwaway leaf —
   it never becomes a daemon-managed mirror, never gains the
   `git_https_remote` origin, and is removed at terminal status
   (success, failure, skip, NeedsAttention) or by the per-tick orphan
   sweep (crash-recovery backstop) with a forensic `.removed` marker.
5. **Review worktree against the quarantine**: the review worktree is
   materialised from the quarantine clone at the standard
   `<repo_storage_root>/worktrees/review/...` path, then the
   quarantine clone is torn down at terminal status.

## Credential path analysis

What the quarantine fetch exposes:

- **PAT scope**: `git fetch <fork_url> <sha>` authenticates with the
  daemon's PAT. Listing a slug in `allow_fork_prs` is an operator
  statement that the PAT's read scope is acceptable for that fork's
  visibility (see the private-fork case below). The fetch does not
  upload anything: the fork learns only that a fetch occurred.
- **askpass helper**: the fetch reuses the hardened GitRunner
  (`--askpass` helper path, no credential persistence).
- **No hooks / git-config persistence**: the quarantine clone is
  created and removed without installing hooks; `git config` writes
  are confined to the throwaway clone's own config file, which dies
  with the clone.
- **No second persistent remote**: the production mirror's remote
  list is untouched by fork review (DAR §11.2).

## Operator opt-in contract

`auto_review.fork_policy.allow_fork_prs` is a **per-repo opt-in list,
default empty (fail-closed)**:

```yaml
auto_review:
  enabled: true
  fork_policy:
    allow_fork_prs:
      - owner/repo
```

- Each slug must be a watched repo; `from_raw` rejects unknown slugs.
- Listing `owner/repo` opts **all** forks of that repo into
  quarantine-path review. The fork's own identity is never consulted
  for the policy decision (it is attacker-controlled).
- **Private forks**: a private fork of an opted-in repo is fetched
  with the daemon's PAT. Only opt a repo in when the PAT's read scope
  is acceptable for the fork's visibility — for a private fork that
  means the PAT can already read it.
- Denied forks keep the Phase-1 behaviour byte-for-byte:
  `review_skipped_fork_unsupported` with the same payload.

## What this does not change

- Non-fork PRs: identical Phase-1 single-origin mirror path.
- The prompt builder, sandbox, and enforcement machinery: unchanged;
  fork content flows through the same untrusted sections and
  mutation-policy enforcement as same-repo PRs.