# Continuous integration

Caduceus runs two GitHub Actions workflows on every pull
request and every push to `main`. Together they are the
required CI matrix; both must be green before a PR can merge
and before a push to `main` is considered a release candidate.

The matrix is owned by `planning/caduceus-v1.0/tasks/1.1-establish-required-ci-matrix.md`
and is the v1.0 contract surface for `CONTRACTS.md` `CI-001`,
`CI-002`, and `CI-003`. Do not edit it without updating the
contract and the v1.0 plan task packet.

## The three required checks

| Job | Workflow | What it runs | Why it is required |
|---|---|---|---|
| `ci / rust-stable` | `.github/workflows/ci.yml` | `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked --all-targets` on stable Rust | The project compiles, lints, and tests against the current stable toolchain. The MSRV is enforced by `Cargo.toml` (`rust-version = "1.97"`) at compile time. |
| `ci / python` | `.github/workflows/ci.yml` | `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py` | The two project-required test files cover the Hermes plugin adapter and the worker bridge; the gate runs exactly that pair. |
| `ci / planning` | `.github/workflows/ci.yml` | `python3 -B planning/caduceus-v1.0/tools/validate_plan.py` | The v1.0 plan validator is the seal that catches an undocumented contract drift, an unsealed v0.1 tree, a missing acceptance ID, or a broken cross-link. |
| `commit-policy / check` | `.github/workflows/commit-policy.yml` | `planning/caduceus-v1.0/tools/check_commit_messages.py --range <base>..<head>` | Enforces the `<type>(<scope>): <description>` shape from `CONTRACTS.md` `CI-003`. A repo that enforces squash merge validates the squash title; otherwise every commit subject is validated. |

The names above are stable. They appear in the branch-protection
settings; renaming a job breaks the required-check contract.

## Matrix

| Workflow | Image | Timeout | Caching |
|---|---|---|---|
| `ci / rust-stable` | `ubuntu-24.04` | 30 min | `~/.cargo/registry`, `~/.cargo/git`, `target` keyed on `Cargo.lock` |
| `ci / python` | `ubuntu-24.04` | 15 min | None (apt + pip only) |
| `ci / planning` | `ubuntu-24.04` | 5 min | None |
| `commit-policy / check` | `ubuntu-24.04` | 5 min | None |

The Rust job installs rustup with the `--profile minimal` flag
and pins the toolchain by SHA-able installer (not `rustup
update`). The `Cargo.lock` is committed; the job uses
`--locked`. There is no `--ignore-rust-version` and no
`RUSTC_BOOTSTRAP=1`; if a contributor needs an unstable
feature they should land it on a feature branch and open a
PR with a tracked issue, not switch the project toolchain.

## Artifact retention

| Artifact | Produced by | Retained |
|---|---|---|
| `target-rust-stable` | `ci / rust-stable` | 14 days |
| `pytest-junit` | `ci / python` | 14 days |

Artifacts are uploaded with `if-no-files-found: ignore` so a
green job that produces no artifact does not fail. Retention is
14 days; raise it in `RELEASING.md` only when a release
investigation needs the build output for longer.

## Trigger filtering

Both workflows use `paths-ignore` so docs-only and
planning-only pushes do not pay the full Rust+Python build
cost. A push that touches only `docs/`, `README.md`,
`CHANGELOG.md`, `CONTRIBUTING.md`, `AGENTS.md`, `RELEASING.md`,
`SECURITY.md`, `LICENSE`, `.github/ISSUE_TEMPLATE/**`,
`planning/**`, or `MIGRATION.md` skips the build entirely. The
`commit-policy` workflow skips too; the `ci` workflow's
planning job still runs to validate the v1.0 catalog.

A change to anything else — Rust source, Python tests,
plugin assets, `Cargo.toml` / `Cargo.lock`, the workflow
files themselves — triggers the full gate.

## Cache warming

The `ci` workflow also has a `schedule: [cron: '0 6 * * 0']`
trigger that runs the full gate every Sunday at 06:00 UTC.
The trigger does not produce a failing-required-check on a
green run (GitHub ignores scheduled runs for branch-protection
purposes) but it does keep the `cargo` registry and `target/`
cache hot. Without the schedule trigger, a human push on
Tuesday after a quiet weekend pays a 5-10 minute cold-cache
build; with it, the build is incremental and finishes in
under two minutes.

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
