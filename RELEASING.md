# Releasing Caduceus

This is the maintainer runbook for a public Caduceus release. Release only
from a reviewed, clean `main` checkout. Do not move or rewrite a published
release tag.

## Versioning

Caduceus follows [Semantic Versioning 2.0.0](https://semver.org/). The public
surface includes the CLI and exit codes, configuration schema, Hermes plugin
manifest, worker environment and result-file contracts, state format, and
documented operator behavior.

- Use a patch release for compatible fixes.
- Use a minor release for compatible additions and documented deprecations.
- Use a major release for a change that requires operator action, including a
  removed or incompatible public interface or state format.

Every breaking state change must ship with a tested migration path and clear
guidance in [MIGRATION.md](MIGRATION.md). Record all operator-visible changes
in [CHANGELOG.md](CHANGELOG.md).

## Prepare the Release

1. Confirm the working tree is clean and all intended changes have passed
   review. Commits must follow the scoped Conventional Commit rules in
   [AGENTS.md](AGENTS.md) and [CONTRIBUTING.md](CONTRIBUTING.md).
2. Choose the SemVer version and update it consistently in `Cargo.toml` and
   `plugin.yaml`.
3. Move the relevant entries from `Unreleased` into a dated version section in
   `CHANGELOG.md`, then add a fresh `Unreleased` section.
4. Update operator documentation and migration instructions for every public
   change. Review [SECURITY.md](SECURITY.md) when the release fixes a security
   issue.
5. Build the release artifact and run the full required gate on the release
   commit:

   ```bash
   cargo fmt --check
   cargo clippy --locked --all-targets -- -D warnings
   cargo test --locked --all-targets
   pytest -q tests/hermes_plugin_test.py tests/bridge_test.py
   cargo build --locked --release
   ```

   Run these commands with the Rust version declared in `Cargo.toml` and from
   a clean checkout. Do not release on a failed, skipped, or waived check.

## Publish

1. Commit the version, changelog, and documentation updates.
2. Create a signed annotated tag for that exact commit:

   ```bash
   git tag -s vX.Y.Z -m "caduceus vX.Y.Z"
   ```

3. Push `main` and the tag without force-pushing:

   ```bash
   git push origin main
   git push origin vX.Y.Z
   ```

4. Create the GitHub release from `vX.Y.Z` and use that version's changelog
   section as its notes. Mark it as the latest release only when it is the
   highest supported non-prerelease.
5. Verify the published tag resolves to the reviewed release commit and that a
   fresh Hermes or standalone installation can build and report the intended
   version. The project does not publish to crates.io.

## After Release

- Confirm the GitHub release, tag, changelog, and installed version agree.
- Watch issue reports and security mail for regressions.
- Keep the next user-visible change under `Unreleased`.

If a release is defective, do not retag or force-push. Publish a follow-up
patch as soon as it is safe, document the impact in the changelog, and update
the GitHub release notes with an operator-facing warning when appropriate.
Handle vulnerabilities through [SECURITY.md](SECURITY.md), not public issues.

## Release Readiness Checklist

Invoke this checklist at the moment of the operator's release decision,
not on every commit. Each row links a release-readiness requirement to a
verification command or artifact the operator can confirm.

| AC ID | Verification command or artifact | Expected result |
|---|---|---|
| 7.6-AC-01 | Run the plan validator; review the acceptance-evidence table in the release handoff | Plan validator reports the published task and phase count; every AC row in the handoff has a green status and a concrete evidence pointer. |
| 7.6-AC-02 | Run the required CI jobs on the release branch; run the pre-merge four-cargo gate locally | All CI jobs pass; the local gate (`cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --all-targets -- --test-threads=1`, `python3 -m pytest -q tests/hermes_plugin_test.py tests/bridge_test.py`) passes. |
| 7.6-AC-03 | Compare the package manifest's version string to the CHANGELOG entry | The manifest and CHANGELOG agree; the CHANGELOG entry is labelled "Internal version bump" and includes the verbatim note that no public release artifacts have been published. |
| 7.6-AC-04 | Compute the SHA-256 of the archived initial-release tree and compare it to the manifest's recorded digest | Both hashes match; the archived tree is byte-for-byte unchanged. |
| 7.6-AC-05 | Review the "Maintainer decision" block in the release handoff | The decision is recorded as `approved` or `blocked`, with the operator's name and the date. |
| 7.6-AC-06 | Review the "Installed path" and "Blocked or unsupported host capabilities" sections of the release handoff | End-to-end-ready claims are forbidden unless a fresh install passes the plugin's setup step on the supported host; blocked or unsupported host capabilities are labelled honestly. |
| 7.6-AC-07 | Run the forbidden-marker greps documented in the release handoff (production-code grep, sentinel-token grep, freshness pre-flight) | All greps return zero hits in the production code, docs, plugin assets, and operator skills (excluding tests, fixtures, and the project TODO list). |

A release is **blocked** if any row reports a red status, any grep returns a hit, the archived initial-release tree has drifted, or the maintainer decision is `blocked`. The release is **ready** when every row is green and the maintainer decision is `approved`.
