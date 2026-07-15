# Contributing

Thanks for helping improve Caduceus. Bug reports, documentation fixes, tests,
and focused pull requests are all welcome.

## Start Here

- Search existing issues and pull requests before opening a new one.
- Use issues for bugs and feature requests; use discussions for design
  questions.
- Report vulnerabilities privately through [SECURITY.md](SECURITY.md), not in a
  public issue.
- Read [AGENTS.md](AGENTS.md) before making a change. It defines the repository
  boundaries, safety rules, and required checks.

## Pull Requests

Keep each pull request focused and explain the problem, the change, and the
tests you ran. Changes to the CLI, configuration, plugin manifest, worker
environment, result schema, or state format may affect the public contract;
consult [RELEASING.md](RELEASING.md) before opening the pull request.

Run the required checks before requesting review:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
pytest -q tests/hermes_plugin_test.py tests/bridge_test.py
```

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

## Project Conventions

- Put tests in `tests/` as `<subject>_test.rs` or `<subject>_test.py`; do not
  add inline test modules under `src/`.
- Use `rustfmt` and Ruff defaults. Keep Markdown links relative, tag fenced
  code blocks, and soft-wrap prose at 80 characters.
- Do not edit daemon state, claim files, or transcripts directly. Use the
  recovery procedures in [docs/state-recovery.md](docs/state-recovery.md).
- Treat `planning/caduceus-v0.1/` as an immutable archive.

## License

By contributing, you agree that your contributions are licensed under the MIT
License.
