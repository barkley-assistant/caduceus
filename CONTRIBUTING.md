# Contributing to Caduceus

Caduceus is an open-source project under the MIT license. We welcome contributions that advance the daemon toward its v0.1 milestone and beyond.

## Status: Pre-implementation

This repository currently contains only planning documents and the project README. **No Rust code has been written yet.** Contributions at this stage are best directed at the planning folder.

## Planning Folder Convention

All design work, scope discussions, and milestone breakdowns live under `planning/`. Each planning document follows this naming convention:

```
planning/YYYY-MM-DD_HHMMSS-<slug>.md
```

Where `<slug>` is a short kebab-case description (e.g., `caduceus-v0.1`, `worker-timeout-design`).

## How to Contribute

### Before opening a PR

1. Read the current planning documents in `planning/`.
2. Check the open issues for the area you want to work on.
3. If your change affects scope or design, propose it in a planning doc **first** — get feedback before writing code.
4. If your change is a bug fix or refactor with no design impact, you can skip the planning doc.

### Code contributions (once implementation begins)

1. Each implementation task in the planning doc has a corresponding test-first sequence.
2. Open PRs should reference the planning task number (e.g., "Implements Phase 5, Task 5.1").
3. Run `cargo test` and `cargo clippy --all-targets -- -D warnings` before requesting review.
4. Commits should follow conventional commits format.

### Documentation contributions

- README improvements, typo fixes, and example additions are always welcome.
- For substantial documentation changes, open an issue first to discuss.

## Design Principles

These are non-negotiable. PRs that violate them will be rejected.

1. **Strict controller-worker separation.** The Rust daemon owns process lifecycle, IO, atomicity, and observability. The worker is replaceable; the daemon is not.
2. **Zero inbound networking.** Caduceus never opens a port or runs an HTTP server.
3. **Secret masking.** Caduceus holds the GitHub token; the worker does not.
4. **Bounded resource usage.** Every worker invocation has a hard timeout. Every issue has a bounded retry budget.
5. **Public voice rule.** When Caduceus posts comments on issues/PRs as a bot, those comments stay generic and don't mention the internal tooling (no "Caduceus", "OpenCode", "Gentle-AI", etc. references in user-facing text).

## Communication

- **Issues:** Use GitHub issues for bug reports and feature requests.
- **Discussions:** Use GitHub Discussions for design questions and broader architectural topics.
- **Security:** For security issues, do not open a public issue. Email security@yourdomain.example (placeholder — replace with real address before public launch).

## License

By contributing to Caduceus, you agree that your contributions will be licensed under the MIT License.