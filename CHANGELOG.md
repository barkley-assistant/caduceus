# Changelog

All notable changes to Caduceus are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/).

## [Unreleased][next]

### Internal version bump

This version bump to 1.0.0 is an internal manifest change. No public
release artifacts have been published. The next refactor pass will
land before any GitHub release is created from this version.

No other unreleased changes.

## [0.1.0] - 2026-07-15

Initial public release. Supports a single host and worker, PAT
authentication, JSON state, dry runs, investigation tickets, and Hermes Agent.

### Added

- Rust daemon, Python reference bridge, and Hermes plugin.
- Dry-run and investigation-ticket workflows.
- Migration with `caduceus migrate-state` and corruption
  recovery with `caduceus recover-state`.
- Worker supervision, claim and heartbeat handling, and a nonblocking
  whole-tick lock.
- An operator migration guide in [MIGRATION.md](MIGRATION.md).

### Known Limitations

- One issue is processed per host-wide tick; a slow worker blocks
  later work.
- Authentication uses a personal access token only.
- State is JSON, written atomically with `temp + fsync + rename`.
- `caduceus status` exit codes were not mapped to the CLI contract;
  every status call exited 0 regardless of outcome.
- The release did not include runtime tests for code and investigation
  success, partial PR-response retry, timeout with a grandchild, or
  concurrent worker execution.

### Security

- Did not publish a security contact or disclosure
  policy.

[next]: https://github.com/barkley-assistant/caduceus/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/barkley-assistant/caduceus/releases/tag/v0.1.0
