# AGENTS.md — Contributor Instructions

These requirements apply to human and automated contributors.

## Repository Boundaries

- Keep this as one Rust crate producing the `caduceus` binary.
- Keep Python in `plugin-assets/worker-bridge.py` and `tests/`.
- Keep the Hermes plugin in `plugin.yaml`, `__init__.py`, and
  `skills/caduceus/`. Hermes Agent v0.18.2 is the minimum host version.
- Do not add top-level directories outside `.github/`, `docs/`, `planning/`,
  `plugin-assets/`, `skills/`, `src/`, and `tests/` without approval.
- Do not commit generated files.

## Safety

- Treat `planning/caduceus-v0.1/` as an immutable archive.
- Never edit daemon state, claim files, or transcripts directly. Use the
  commands in `docs/state-recovery.md`.
- Never commit operator-generated prompt files or directories.
- Never change `contracts_sha256` or archived digests to bypass validation.
  Record contract conflicts in `CONTRACT_REVISIONS.md` for human approval.
- Do not add `todo!()`, `unimplemented!()`, or new `unsafe` production code.
- All Caduceus-created pull requests require human review and merge.

## Commits

Every commit and merge commit follows
[Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/)
with a required, non-empty scope:

```text
<type>(<scope>): <description>
```

Type and scope are lowercase. The description is imperative, has no trailing
period, and keeps the complete subject at 80 characters or fewer. For example:
`feat(lang): add Polish language example`. Use the type and scope appropriate
to the actual change; the example is not a prescribed value.

Put tests in `tests/` as `<subject>_test.rs` or `<subject>_test.py`; do not add
inline test modules under `src/`.

Run before pushing:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
pytest -q tests/hermes_plugin_test.py tests/bridge_test.py
```

Use `rustfmt` and Ruff defaults. Keep Markdown links relative, tag fenced code
blocks, and soft-wrap prose at 80 characters.
