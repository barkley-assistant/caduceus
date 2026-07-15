# Installation

The daemon has one binary (`caduceus`), one Python bridge
(`worker-bridge.py`), and one Hermes plugin manifest
(`plugin.yaml`). The install path differs depending on
whether you're using Hermes Agent or running the daemon
standalone. Both paths produce the same daemon at runtime;
only the surrounding plumbing differs.

## Prerequisites (Both Paths)

- **Rust toolchain at the pinned MSRV.** The
  `Cargo.toml` declares `rust-version = "1.97"`. Earlier
  toolchains will fail at the `--locked` step. The
  project's CI configuration is not yet defined (see
  `docs/README.md` §"To Be Added").
- **Git.** The daemon shells out to Git; the project does
  not use libgit2. SSH or a credential helper must be
  configured for noninteractive fetch and push.
- **Python 3.11+.** The bridge is Python; the reference
  harness in `plugin-assets/worker-bridge.py` is Python.
- **A GitHub account with a fine-grained Personal Access
  Token (PAT).** GitHub App authentication with installation
  tokens is a future feature.

## Supported Hosts

Caduceus is one Rust daemon plus one Python bridge. The
host tier table mirrors the README's front-door promise:

- **Linux — tier 1.** Primary target. CI, the release
  canary, and the installed-path proof run on Linux.
  Every documented behaviour is exercised there.
- **macOS — works, supported.** The supervisor and Git
  runner are portable; macOS uses the same paths as
  Linux. Filed regressions are accepted but not
  exercised in CI before a release.
- **Windows — not a target.** The supervisor relies on
  POSIX process-tree semantics that the Windows job
  objects do not provide. There is no support roadmap
  and no plan to ship a Windows build. Operators who
  want Windows-native execution should run Caduceus in
  WSL2.

If a host is not listed here, it is not supported. See
the README's front-door version of this section for the
one-paragraph framing.

## Path 1: Hermes Agent (Recommended)

This is the install path most operators use. Hermes
discovers the plugin, runs `hermes caduceus setup` for
you, and wires up cron plus the chat status command.

```bash
# Hermes Agent v0.18.2 or newer
hermes plugins install barkley-assistant/caduceus --enable
hermes caduceus setup                 # build + seed your bridge
hermes caduceus cron-install          # 2-min no-agent job
hermes caduceus status                # verify
```

### What `hermes caduceus setup` Does

`setup` is the only command that mutates the host
filesystem beyond installing the plugin source. It is
idempotent.

1. Verifies the Rust toolchain, Git, and Python
   prerequisites.
2. Runs `cargo build --release --locked
   --manifest-path <plugin>/Cargo.toml`.
3. Atomically installs the resulting binary as
   `<plugin>/bin/caduceus` with mode 0755.
4. Creates the configured state directories with mode
   0700 (state, runs, claims, cache) under
   `$HERMES_HOME/caduceus-state/` by default.
5. Seeds the user-owned bridge at
   `$HERMES_HOME/caduceus/worker-bridge.py` (default
   `~/.hermes/caduceus/worker-bridge.py`) — only if
   absent. If the shipped bridge template has changed
   since you installed, setup writes a sibling `.new`
   candidate and tells you; it never overwrites your
   edits.

### What `hermes caduceus cron-install` Does

1. Atomically writes a generated Bash wrapper at
   `$HERMES_HOME/scripts/caduceus-pulse.sh` containing
   the absolute installed binary path and
   `exec <binary> run`.
2. Creates or reconciles exactly one named `caduceus`
   no-agent cron job equivalent to:

   ```text
   hermes cron create "every 2m" --name caduceus \
       --script caduceus-pulse.sh --no-agent
   ```

   Reconciliation goes through Hermes's registered
   `cronjob` tool, not direct edits to `cron/jobs.json`.
   Zero matches creates; one match updates/reuses;
   multiple matches fail with their IDs.

3. The gateway (or a configured managed cron provider)
   must be running for the job to fire. See
   `hermes-integration.md`.

### Update

```bash
hermes plugins update caduceus        # refresh source
hermes caduceus setup                 # rebuild binary
```

`hermes caduceus setup` is what actually moves the new
binary into place. Plugin-source updates do not rebuild
themselves; that is by design — Hermes never runs
manifest build steps.

### Uninstall

```bash
hermes caduceus cron-remove           # removes the cron job + wrapper
hermes plugins remove caduceus        # tears down the plugin
```

Your state directory, your user-owned bridge, your
`~/.hermes/config.yaml` caduceus section, and your
watched repositories all survive. Reinstall with
`hermes plugins install` against the same state and the
daemon resumes where it left off.

## Path 2: Standalone (No Hermes)

Use this when you don't want Hermes, or when Hermes is
inconvenient (a containerised worker host, a CI runner,
an air-gapped test rig). You lose the plugin's skill,
the chat `/caduceus-status` slash command, and the
Hermes cron integration; you keep the daemon.

```bash
git clone https://github.com/barkley-assistant/caduceus
cd caduceus
cargo build --release --locked
install -m 0755 target/release/caduceus ~/.local/bin/caduceus

# write your config at ~/.config/caduceus/config.yaml
# see configuration.md for the schema

# install a cron job yourself; caduceus does not do it for you
# example for crontab:
#   */2 * * * *  ~/.local/bin/caduceus run \
#     >> ~/.local/share/caduceus/cron.log 2>&1
```

A standalone install **requires** an explicit
`worker_command` in the config. The daemon refuses to
start without it. This is on purpose: the Hermes plugin
has a default bridge path; you don't, so the daemon
makes you say it out loud.

## The Cron Contract

Regardless of which install path you used, the daemon's
cron expectation is the same:

- The cron runs `caduceus run` (or no-argument
  `caduceus`, which is rewritten to `caduceus run`
  before clap parsing).
- `caduceus run` is silent on stdout for every cron
  contract outcome: processed, idle, concurrent,
  cadence, rate-limited, cancelled. Diagnostics go to
  stderr. Cron captures nothing on success.
- Exit 0 for every cron contract outcome; exit 1 for
  configuration, corruption, invariant, or unrecovered
  pipeline failures.
- The daemon's whole-tick flock makes it safe to run on
  multiple schedulers (Hermes plus system cron plus a
  manual `caduceus run` from your shell at the same
  time); only one tick wins the lock and the others
  exit 0 with the "concurrent tick" outcome.

## Uninstalling Without Reinstalling

If you want to remove Caduceus entirely:

- **Hermes path:** `hermes caduceus cron-remove &&
  hermes plugins remove caduceus` then delete
  `$HERMES_HOME/caduceus-state/` and `~/.hermes/caduceus/`
  if you don't want to keep state.
- **Standalone path:** remove the crontab line, `rm` the
  binary, `rm -rf` the state directory and the config.

The watched repositories are unrelated to Caduceus;
remove them with their own tooling.
