# Releasing

This is the maintainer runbook for a public Caduceus release.
Release only from a reviewed, clean `main` checkout. Do not
move or rewrite a published release tag — a bad release gets
a follow-up patch, never a retag.

## Versioning

Caduceus follows [Semantic Versioning 2.0.0](https://semver.org/).
The versioned surface — what a version number promises
something about — is:

- **The `caduceus` CLI.** Subcommands, flags, exit codes,
  and the `--json` output shape of `status`.
- **The `Config` YAML schema.** Field names, types, and
  defaults under the `caduceus:` block. `serde` rejects
  unknown fields on purpose: an unknown key is a config
  error, never a silent ignore.
- **The plugin manifest.** `plugin.yaml` fields the Hermes
  host loads: commands, skill references, cron contract.
- **The worker contract.** The `CADUCEUS_*` environment
  variables, the `worker-result.json` schema, and the
  exit-code semantics. Your `worker-bridge.py` is your own
  file; the contract it speaks is versioned.
- **The state format.** `state.json` / `state_meta.json`
  and the SQLite schema. The daemon validates both at open
  and refuses to run against an unknown schema.
- **The default `comment_forbidden_strings`.** The
  public-voice rule's default list is part of the surface;
  changing what the daemon refuses to say by default is a
  breaking change.

A breaking change is any change to the surface above that
could make an existing installation behave differently
without the operator opting in.

- Use a **patch** release for compatible fixes.
- Use a **minor** release for compatible additions and
  documented deprecations.
- Use a **major** release for a change that requires
  operator action — a removed or incompatible public
  interface, or a state format change. Every breaking
  state change must ship with a tested migration path and
  clear guidance, documented before the release is tagged.

Record all operator-visible changes in
[CHANGELOG.md](CHANGELOG.md).

## Prepare the release

1. **Green main.** The tip of `main` passes CI and your
   working tree is clean. Commits follow the scoped
   Conventional Commit rules in `AGENTS.md` /
   `CONTRIBUTING.md`.
2. **Bump the version.** `Cargo.toml` is the source of
   truth (`version = "X.Y.Z"`). Bump `plugin.yaml` too if
   the plugin surface changed. Commit as
   `chore(release): bump to vX.Y.Z`.
3. **Finalise the changelog.** In `CHANGELOG.md`, rename
   `## [Unreleased]` to `## [X.Y.Z] - <date>`, open a fresh
   empty `## [Unreleased]`, and fix the compare links at
   the bottom (`[Unreleased]` → `compare/vX.Y.Z...HEAD`,
   `[X.Y.Z]` → `releases/tag/vX.Y.Z`). Commit.
4. **Document and gate.** Update operator documentation for
   every public change. Review `SECURITY.md` when the
   release fixes a security issue. Run the full gate on the
   release commit:

   ```bash
   cargo fmt --check
   cargo clippy --locked --all-targets -- -D warnings
   cargo test --locked --all-targets
   python3 -m pytest -q tests/hermes_plugin_test.py tests/bridge_test.py
   cargo build --locked --release
   ```

   Run these with the Rust version declared in `Cargo.toml`
   from a clean checkout. Do not release on a failed,
   skipped, or waived check.

## Publish

1. Commit the version, changelog, and documentation
   updates.
2. Create a signed annotated tag for that exact commit:

   ```bash
   git tag -s vX.Y.Z -m "caduceus vX.Y.Z"
   ```

3. Push `main` and the tag without force-pushing:

   ```bash
   git push origin main
   git push origin vX.Y.Z
   ```

4. The `release` workflow (`.github/workflows/release.yml`)
   does the rest — note that CI does *not* run for tags, so
   this workflow is the tag's only gate. It verifies the tag
   matches `Cargo.toml` (a mismatch fails the run — that's
   the point), runs the gates, builds `--release --locked`,
   packages `caduceus-<tag>-x86_64-unknown-linux-gnu.tar.gz`
   plus `SHA256SUMS`, slices the matching `## [X.Y.Z]`
   section out of `CHANGELOG.md`, and publishes the GitHub
   release with those notes and artifacts. Mark it as the
   latest release only when it is the highest supported
   non-prerelease.
5. **Verify.** The published tag resolves to the reviewed
   release commit; the release notes match the changelog
   section; the tarball and checksum are attached. Download,
   unpack, `./caduceus --version` — it should print the
   released version. The project does not publish to
   crates.io.

If a step fails before publishing: fix, merge to `main`,
delete the tag (`git push origin :refs/tags/vX.Y.Z`), retag
from the new tip. Tags are cheap until they are published.

## After release

- Confirm the GitHub release, tag, changelog, and installed
  version agree.
- Watch issue reports and security mail for regressions.
- Keep the next user-visible change under `[Unreleased]`.

If a release is defective, do not retag or force-push.
Publish a follow-up patch as soon as it is safe, document
the impact in the changelog, and update the GitHub release
notes with an operator-facing warning when appropriate.
Handle vulnerabilities through `SECURITY.md`, not public
issues.
