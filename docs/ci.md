# Continuous integration

Caduceus runs two GitHub Actions workflows on every pull
request and every push to `main`. Together they are the
required CI matrix; both must be green before a PR can merge
and before a push to `main` is considered a release candidate.

The matrix is owned by `planning/caduceus-v1.0/tasks/1.1-establish-required-ci-matrix.md`
and is the v1.0 contract surface for `CONTRACTS.md` `CI-001`,
`CI-002`, and `CI-003`. Do not edit it without updating the
contract and the v1.0 plan task packet.

## The four required checks

| Job | Workflow | What it runs | Why it is required |
|---|---|---|---|
| `ci / rust-1.97` | `.github/workflows/ci.yml` | `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked --all-targets` on Rust 1.97 (the pinned MSRV) | The project compiles, lints, and tests against the lowest-supported toolchain so a v1.0 operator on the MSRV does not hit a surprise. |
| `ci / rust-stable` | `.github/workflows/ci.yml` | Same four-line gate on stable Rust | The daemon also has to work on the current stable toolchain; the MSRV gate does not catch a feature that compiles on stable but not on 1.97. |
| `ci / python` | `.github/workflows/ci.yml` | `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py` | The two project-required test files cover the Hermes plugin adapter and the worker bridge; the gate runs exactly that pair. |
| `ci / planning` | `.github/workflows/ci.yml` | `python3 -B planning/caduceus-v1.0/tools/validate_plan.py` | The v1.0 plan validator is the seal that catches an undocumented contract drift, an unsealed v0.1 tree, a missing acceptance ID, or a broken cross-link. |
| `commit-policy / check` | `.github/workflows/commit-policy.yml` | `planning/caduceus-v1.0/tools/check_commit_messages.py --range <base>..<head>` | Enforces the `<type>(<scope>): <description>` shape from `CONTRACTS.md` `CI-003`. A repo that enforces squash merge validates the squash title; otherwise every commit subject is validated. |

The names above are stable. They appear in the branch-protection
settings; renaming a job breaks the required-check contract.

## Matrix

| Workflow | Image | Timeout | Caching |
|---|---|---|---|
| `ci / rust-1.97` | `ubuntu-24.04` | 30 min | `~/.cargo/registry`, `~/.cargo/git`, `target` keyed on `Cargo.lock` |
| `ci / rust-stable` | `ubuntu-24.04` | 30 min | Same |
| `ci / python` | `ubuntu-24.04` | 15 min | None (apt + pip only) |
| `ci / planning` | `ubuntu-24.04` | 5 min | None |
| `commit-policy / check` | `ubuntu-24.04` | 5 min | None |

Both Rust jobs install rustup with the `--profile minimal` flag
and pin the toolchain by SHA-able installer (not `rustup
update`). The MSRV job pins `1.97.0`; the stable job pins
`stable`. The `Cargo.lock` is committed; both jobs use
`--locked`. There is no `--ignore-rust-version` and no
`RUSTC_BOOTSTRAP=1`; if a contributor needs an unstable
feature they should land it on a feature branch and open a
PR with a tracked issue, not switch the project toolchain.

## Artifact retention

| Artifact | Produced by | Retained |
|---|---|---|
| `target-rust-1.97` | `ci / rust-1.97` | 14 days |
| `target-rust-stable` | (future) | 14 days |
| `pytest-junit` | `ci / python` | 14 days |

Artifacts are uploaded with `if-no-files-found: ignore` so a
green job that produces no artifact does not fail. Retention is
14 days; raise it in `RELEASING.md` only when a release
investigation needs the build output for longer.

## Cancel-in-progress

Both workflows use `concurrency: ci-${{ github.ref }} /
cancel-in-progress: true`. A force-push on a PR cancels the
previous run so the runner does not burn CI minutes against a
now-stale head SHA. The `commit-policy` workflow uses the same
pattern with its own concurrency group.

## Local reproduction

The CI workflows are the same commands the operator runs
locally before pushing. From the repository root:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 -m pytest -q tests/hermes_plugin_test.py tests/bridge_test.py
python3 -B planning/caduceus-v1.0/tools/validate_plan.py
```

The `commit-policy` job maps to:

```bash
python3 planning/caduceus-v1.0/tools/check_commit_messages.py \
  --range <base-sha>..<head-sha>
```

for a PR that preserves commits, or:

```bash
python3 planning/caduceus-v1.0/tools/check_commit_messages.py \
  --squash-title "<the PR's squash title>"
```

for a repo that enforces squash merge.

## Why two workflows, not one

`ci.yml` runs the build / test / plan gate. `commit-policy.yml`
runs the commit-message gate. Splitting them means a commit
that violates `CI-003` does not waste a full Rust build, and a
build that fails does not waste a second runner on the commit
gate. The two workflows share the same triggers (`pull_request`
and `push: branches: [main]`) so a contributor sees the same
green / red surface regardless of which workflow flagged the
problem.
