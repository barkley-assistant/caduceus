# FAQ

Short answers to short questions. If you don't find yours
here, `troubleshooting.md` is the long-form table; the
README is the orientation.

**Is Caduceus a GitHub App?**

No. Caduceus is PAT-only. GitHub App authentication with
installation tokens is a future feature.

**Does Caduceus replace code review?**

No. Every PR Caduceus opens is opened for a human to
review and merge. There is no auto-merge today.
Policy-gated auto-merge with a documented policy in plain
English is a future feature.

**Does Caduceus run on Windows?**

No. Caduceus targets Unix (Linux tier-1; macOS works
because the supervisor is portable). Windows is not a
target.

**Does Caduceus expose an HTTP API?**

No. Caduceus is pull-only. It polls GitHub on a schedule.
The project will never accept inbound HTTP. If you want
push semantics, write a webhook → label-relabel shim in
front of Caduceus; that's your shim, not the daemon's.

**Can I use Caduceus without Hermes?**

Yes. See `installation.md`. You lose the plugin skill,
the chat status command, and the Hermes cron integration;
the daemon is the same.

**Can I use Caduceus with multiple harnesses at once?**

The worker contract is one bridge per daemon. If you
want to A/B test harnesses, run two daemons with different
`worker_command` configs against the same GitHub org. This
is not a recommended deployment; the daemon's host-wide
lock means only one of them wins any given tick. For real
harness diversity, run them on different issue labels.

**Can I run two Caduceus daemons on the same host?**

Yes, against different state directories and different
labels. Don't run two daemons against the same state
directory; the whole-tick lock makes that safe but
pointless, and the queue store isn't built for it.

**Can I run two Caduceus daemons on different hosts against the same GitHub
org?**

No. Both daemons will poll the same issues and one will
lose every race. Multi-host state with leader election is
post-v0.1.

**Does Caduceus need a database?**

No. JSON state. Caduceus is single-host with the
state-directory-as-database model.

**Does Caduceus need a hosted backend?**

No. Caduceus is a single binary on your machine. Your
state is on your disk. Your credentials never leave your
host. If you want a hosted alternative, several exist;
the project is not them.

**Why is the worker contract Python?**

It isn't. The daemon treats the worker as a black box
that reads `CADUCEUS_*` env vars and exits with a code.
The reference implementation is Python because Hermes
plugins are conventionally Python and Python is the
lowest-friction language for harness integration. You can
write your bridge in any language the harness's
invocation needs.

**Can I write the bridge in Bash?**

You can. The project does not recommend it for non-trivial
harnesses because signal handling and process-group
discipline are harder to get right in Bash, but a 30-line
Bash bridge for a simple harness is fine.

**Why does my comment get rejected for "caduceus" when I didn't write that?**

Either the bridge wrote it (the harness's template may
have included the daemon's name and the harness echoed
it back), or the issue's body contained the string and
the harness's summary referenced it. See
`public-voice.md`; the substring match is
case-insensitive.

**Why does `caduceus status --json` say `state_corrupt: true`?**

`<state_dir>/state.json` (or `state_meta.json`) failed
validation. **Do not edit the file in place.** See
`state-recovery.md`.

**How do I migrate from v0.1?**

See `MIGRATION.md` at the repository root. The v0.1
upgrade path uses the same `caduceus migrate-state`
command as the v0 → v0.1 path did.

**How do I migrate from a legacy cron processor?**

Also `MIGRATION.md`. Same command.

**How do I contribute?**

`CONTRIBUTING.md`. The short version: open an issue
first for any change that affects the contract surface;
use the required scoped Conventional Commit format for
every commit and merge commit; pass
the canonical gate (`cargo fmt`, `cargo clippy --locked
--all-targets -- -D warnings`, `cargo test --locked
--all-targets`, `pytest tests/hermes_plugin_test.py
tests/bridge_test.py`); respect `AGENTS.md`.

**How do I contact you?**

Security issues: see `SECURITY.md`. Everything else:
GitHub issues.
